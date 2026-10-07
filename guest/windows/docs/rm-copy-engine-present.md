# The windowed Present copy on an RM copy-engine channel: feasibility findings

Status: sections 0 to 9 are the research (M0) and the M1 tool, which PASSed on a GB202 (async CE, acquire held, verify ok).
M3a, the host-testable half of the KMD route, is
sections 10 and 11 (`protocol/src/rm_fence_v3.rs`, `kmd_logic/src/ce_present.rs`). M3b, the KMD's own copy-engine
channel and its hardware self-test (`RmCopyEngine` = 2), is built (11.9, 11.10); M3c-0, the record's parse, is built
(13); M3c-2, the Present route (`RmCopyEngine` = 1), is built, stub-checked only and has not run (15); its producer dup
(`ce_dup.rs`) is a stub until the M3c-1 branch's real module is merged. M1b, the tool's block-linear round trip, ran on hardware and PASSed (10.4); M1c, the RGBA -> BGRA
conversion inside the copy with the CE remap unit (section 12), also PASSed on hardware, including a block-linear source.
Every claim cites the file or patch it comes
from; "unknown" marks what nobody has run, with what would settle it. Host driver release assumed: 610.57.04 (the release
the host backend runs, `host/backend/gen/src/rmallow/v610_57_04.rs`). GPU: RTX 5090 (GB202) unless Ada is named.

## 0. The question and the short answer

A windowed legacy-blt Present today pays a serial chain (`kmd-handoff-2026-10.md` section 1): the KMD defers the copy until the
producer (the NVK app's GPU work) is done (mean 0.61 ms, `BltDeferUs / BltAsyncDefer`), then submits a Venus ring-1 copy from the
app's image into the Blt destination (a guest-memory blob over the destination's own system pages, `zero-copy-present.md` 24.12)
and waits for its completion (0.5-1 ms, of which the GPU copy is 0.2 ms, `BltAsyncLat0..7`). The proposal: the KMD's own RM client
owns a copy-engine (CE) channel; per Present it writes one push buffer that (1) ACQUIREs the producer's RM semaphore, so the GPU
waits instead of a KMD worker, (2) copies with the CE into the destination pages described to RM as an OS-descriptor system-memory
object, (3) RELEASEs a KMD semaphore that retires the Present's DMA fence. Submission is a doorbell write at Present time.

**Verdict.** Feasible on the host as it is: every RM class and control a CE channel needs is in the 610.57.04 allowlist, the
allowlist does not tell a KMD-owned client from a user-mode one, and the doorbell is a direct MMIO write through the host window.
What does not exist is the guest side: the KMD's RM client has no channel, VA space, semaphore or event code. Also, the KMD learns
the producer's fence only as an opaque host fence handle, not as a GPU address it could ACQUIRE on. Both can be proven first
in user mode with a librmclient smoke tool (section 5) before any KMD work.

## 1. What exists

### 1.1 NVK-on-RM already builds channels (the reference sequence)

The exec context of `nvkmd_rm_ctx.c` (`guest/nvk-rm/patches/0006-nvk-rm-execution-and-bind-contexts.patch` 543-699; summary in
`guest/nvk-rm/README.md` 1106-1121), with the fixes of patch 0008:

| step | RM object / call | where |
|---|---|---|
| client | `NV01_ROOT_CLIENT` 0x41, one per `VkDevice` | 0002:1104, 0004:188-193 |
| device | `NV01_DEVICE_0` 0x80, `vaMode = OPTIONAL_MULTIPLE_VASPACES`, 64 KiB big pages | 0004:196-203, 0008:144-149 |
| subdevice | `NV20_SUBDEVICE_0` 0x2080 | 0004:210-215 |
| VA space | `FERMI_VASPACE_A` 0x90f1 with `index = GPU_DEVICE` (a new VA space left GR context buffers nowhere to go under GSP) | 0008:19-25, 169-179 |
| doorbell | highest `*_USERMODE_A` under the subdevice (0xc761 on GB20x, `{bBar1Mapping=1, bPriv=0}`), CPU-mapped write-only, 64 KiB | 0003:251-263, 0004:83-112 |
| classes | the highest per family from `NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2` | 0003:228-249 |
| channel group | `KEPLER_CHANNEL_GROUP_A` 0xa06c `{hVASpace, engineType = GRAPHICS}` | 0006:598-608 |
| subcontext | `FERMI_CONTEXT_SHARE_A` 0x9067, SYNC (VEID 0) | 0006:610-621, 0008:562-569 |
| channel | GPFIFO class under the TSG, `NV_CHANNEL_ALLOC_PARAMS{hObjectError, gpFifoOffset, gpFifoEntries = 1024, hContextShare, hUserdMemory[0], userdOffset[0] = 4096, engineType}` | 0006:623-638 |
| bind | `NVA06F_CTRL_CMD_BIND` before any engine object (GSP refuses them otherwise) | 0008:596-611 |
| engine objects | 3D, compute and copy with NULL params on the same channel ("RM picks the GR copy engine") | 0006:518-541, 640-657 |
| token | `NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX`, then `GET_WORK_SUBMIT_TOKEN` | 0006:659-679 |
| schedule | `NVA06C_CTRL_CMD_GPFIFO_SCHEDULE {bEnable = 1}` on the TSG | 0006:681-688 |

Memory: the GPFIFO ring (1024 x 8 B), push slots and a context semaphore area are one GART allocation over NVK's own pages
(`NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`, fallback `NV01_MEMORY_SYSTEM`) (0006:150-159, 561-578; 0004:390-416; 0008:59-73). The error
notifier (offset 0) and USERD (offset 4096) are 8 KiB of RM-allocated `NV01_MEMORY_SYSTEM`, because RM writes the notifier itself
(0006:68-72, 161-166, 580-596, 921-944). Submission has no RM call: GPFIFO entry, `GP_PUT` into USERD (+0x8c), then the token to
`usermode + 0x90` (0006:287-293, 385-399). USERD `GP_GET` is never written back on GB202 under GSP. Each kick therefore ends with
a no-WFI semaphore release of a sequence number, and ring progress is read from that (patch 0009:9-15, 72-101, 145-198).

**There is a second-engine precedent.** Patch 0035 (`patches-windows/0035-nvk-rm-video-decode-on-an-NVDEC-channel.patch`
68-120) builds the same TSG -> subcontext -> channel -> BIND sequence with `engineType = NV2080_ENGINE_TYPE_NVDEC0`, one engine
object on the channel, unchanged token/schedule/USERD code, and syncs kept as host-class semaphore methods "which every runlist
executes" (0035:20-21). A CE channel is the same with `NV2080_ENGINE_TYPE_COPY(n)` and a CE object allocated with
`NVB0B5_ALLOCATION_PARAMETERS{version, engineType}` (8 bytes). That is an inference: NVK-on-RM never builds a separate CE channel.
`has_transfer_queue` is false everywhere (0003:357, 0035:152), and `NV2080_ENGINE_TYPE_COPY0` is defined but unused (0002:614). The
README lists "transfer queue (async CE channel)" as missing (`guest/nvk-rm/README.md` 1145).

### 1.2 Classes per GPU, and the host allowlist (610.57.04)

| object | GB202 (Blackwell) | AD10x (Ada) | allowlist row (`v610_57_04.rs`) |
|---|---|---|---|
| GPFIFO channel | `BLACKWELL_CHANNEL_GPFIFO_B` 0xca6f | `AMPERE_CHANNEL_GPFIFO_A` 0xc56f | 914 / 881 (376 B, required) |
| copy engine | `BLACKWELL_DMA_COPY_B` 0xcab5 | `AMPERE_DMA_COPY_B` 0xc7b5 | 915 / 899 (8 B, optional) |
| doorbell | `BLACKWELL_USERMODE_A` 0xc761 | `AMPERE_USERMODE_A` 0xc561 | 895 / 880 |
| channel group, subcontext, VA space | 0xa06c, 0x9067, 0x90f1 | same | 850, 838, 849 |
| memory | `NV01_MEMORY_SYSTEM` 0x3e, `OS_DESCRIPTOR` 0x71, `NV50_MEMORY_VIRTUAL` 0x50a0 | same | 798, 804, 833 |
| semaphore surface, OS event | 0xda, `NV01_EVENT_OS_EVENT` 0x79 | same | 813, 807 |

The class choice is NVK's (`guest/nvk-rm/README.md` 1043-1047, `docs/GPU-SUPPORT.md` 26-28). `BLACKWELL_*_A` (0xc96f, 0xc9b5)
are GB10x classes and are allowed too (906, 909). The controls are all allowed, with exact parameter sizes:
- `GPU_GET_CLASSLIST_V2` 0x00800292 (143), `GPU_GET_ENGINES_V2` 0x20800170 (268)
- `CE_GET_CAPS_V2` 0x20802a03 and `CE_GET_ALL_CAPS` 0x20802a0a (470-471)
- `NVA06C GPFIFO_SCHEDULE` 0xa06c0101 (638), `NVA06F BIND` 0xa06f0104 (646)
- `NVC36F GET_WORK_SUBMIT_TOKEN` 0xc36f0108 and `SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX` 0xc36f010a (730-731)
- the semaphore-surface controls 0xda0001-0xda0006 (185-190), `EVENT_SET_NOTIFICATION` 0x20800301 (310) and `TIMER_GET_TIME`
  0x20800403 (320)

**Nothing is missing from the table for this route.** Enforcement is per class and per control, with exact sizes
(`host/backend/device/src/nvidia/ioctl.rs` 87-166, 748-808). It knows no guest OS and no owner: the backend tracks file kind and
client handles only (`ioctl.rs` 304-338). A tool on the Helios RM path or the KMD's own client gets what NVK gets.
`MAP_MEMORY_DMA` has no allowlist check, plain passthrough (`ioctl.rs` 853-866).

Caveat: on a host release with no exact table, only entries every release agrees on pass (`rmallow/mod.rs` 333-414). The channel
and VA-space sizes differ between releases (0xc56f is 360/368/376 B, 0x90f1 48/56, 0xa06c 20/28), so an unlisted release would
refuse them.

### 1.3 What the KMD's own RM client implements, and what it would need

The KMD's client (`kmd_render/src/virtio/rm_client.rs`, the step machine at 782-846; `kmd-rm-client.md` 4, sequence table 158-185)
holds about five or six handles (`kmd-rm-client.md` 100-102):
- a client, a device and a subdevice;
- vidmem (`NV01_MEMORY_LOCAL_USER`) exported to a GEM handle for scanout;
- at level 5, `NV01_MEMORY_SYSTEM` (`virtio/rm_client/sysmem.rs` 712-790);
- CPU mappings, through `RM_MAP_MEMORY` plus a host `Mmap` into the RM window, then `MmMapIoSpace` (`rm_client.rs` 1163-1284).

Every call is a forwarded wire message with owner `KMD_RM` on the control queue (`rm_client.rs` 650-676, `virtio/ctrl.rs` 615), at
about 55 us per call (`kmd-handoff-2026-10.md` 83). It has no VA space, channel group, channel, CE object, semaphore surface, OS
event or GPU mapping (`docs/HANDOFF.md` 100-103). It also never creates a fence (`kmd-rm-client.md` 119-120).

| needed | exists in the KMD client | gap |
|---|---|---|
| client, device, subdevice | yes | the device may need `OPTIONAL_MULTIPLE_VASPACES` (unknown, see 6) |
| VA space + GPU mappings (`NV50_MEMORY_VIRTUAL`, `MAP_MEMORY_DMA`) | no | new steps; librmclient's `crm_map_dma2` is the model (`rmclient.h` 89) |
| TSG, subcontext, channel, BIND, CE object, token, schedule | no | new steps (section 1.1 order) |
| USERD + error notifier (RM sysmem, CPU-written `GP_PUT`) | the sysmem alloc and the window mapping exist | reuse `sysmem.rs` `alloc_sys` and the map steps |
| GPFIFO ring + push buffer | no | KMD nonpaged pages as an OS descriptor (1.4), or RM sysmem through the window |
| doorbell map | the map path exists (vidmem/sysmem) | a usermode object under the subdevice; `crm_smoke` maps it the same way (`tests/crm_smoke.c` 164-191) |
| OS descriptor from KMD pages | only on behalf of user mode (PIN, `ddi/escape.rs` 2476, `virtio/nvrm.rs` 793 `pin_pages`, `kmd_logic/src/page_runs.rs`) | a KMD-owned registration with the same page-run block |
| cross-client dup of the source image and the producer semaphore | no | `NV_ESC_RM_DUP_OBJECT` from the KMD's client (2.3) |
| completion event | the KMD routes host `EventReady` to registrations (`kmd_logic/src/nvrm_events.rs`) | an OS event owned by the KMD client, or a KMD semaphore surface + fence (4.3) |

### 1.4 Smoke tools that exist

The tools live in `guest/rmclient/tests`, are built by meson and never run by `meson test` (`guest/rmclient/meson.build` 55-81).
None of them builds a channel, and nothing outside the Mesa patches does:
- `crm_smoke`: device, subdevice, `FERMI_VASPACE_A`, local and system memory, usermode map, OS event.
- `crm_pin_smoke`: an OS descriptor over process pages, GPU-mapped, at 2 MiB and 512 MiB.
- `crm_semsurf_smoke`: an `NV_SEMAPHORE_SURFACE` plus a host fence fired by a CPU `SET_VALUE`, with latency statistics. It is the
  closest to an acquire/release.
- `crm_event_smoke`, `crm_scanout_smoke` and `crm_share_smoke`; `crm_share_smoke` dups memory across clients and processes.

## 2. The semaphore ACQUIRE on the producer's fence

### 2.1 How the producer's completion is represented today

Patch 0030 (`patches-windows/0030-nvk-rm-wsi-presents-retire-on-RM-fences-no-CPU-wait-.patch`; `guest/nvk-rm/README.md` 1034-1060)
works like this:
1. Each presenting queue has a 64-bit timeline semaphore that is one entry (32 B on GB20x, value at offset 0) of an
   `NV_SEMAPHORE_SURFACE`. The surface lives under NVK's subdevice, over 4 KiB of RM-allocated `NV01_MEMORY_SYSTEM`. An OS
   descriptor is refused there with `NOT_SUPPORTED` (0030:11-13, 786-818).
2. NVK's 3D channel signals it with a host `SEM_EXECUTE` release (64-bit, WFI) plus `NON_STALL_INTERRUPT`. The channel is bound to
   the surface with `BIND_CHANNEL` 0xda0002 (0030:534-597).
3. Per Present, nvidia-drm `SEMSURF_FENCE_CTX_CREATE` (0x54, nested `{hClient, hSemaphoreSurface, ...}`, index = entry) runs once
   per timeline. Then `SEMSURF_FENCE_CREATE` (0x55) is called for the next value and returns a host backend handle. The KMD records
   it as `DEVICE_TYPE_FENCE` 511 (`kmd_logic/src/nvrm_fence.rs` 39-121), and the host sends one `EventReady` when the value lands.

The Present carries only that handle: `HeliosRmFenceTail {rm_fence_handle u32, flags u32, rm_fence_value u64}` grows HERF to 48 B
and HEPR to 96 B, and the value is diagnostic only (`protocol/src/rm_fence.rs` 108-140, `rm-fence-marker.md`). Further steps:
- At `DxgkDdiRender`, `attach_rm_fence_marker` (`ddi/submit_command.rs` 1702-1731) moves the handle into a per-process RM gate.
  `rm_gate_attach` (`virtio/gpu/rm_gates.rs` 195-268) returns a present-stream boundary.
- The `nvrm_events` DPC fires the gate (`virtio/gpu/nvrm_events.rs` 477-503, 560; `rm_gates.rs` 301-320).
- `scanout_boundary_ready` (`virtio/gpu/mod.rs` 7120-7128) and `present_stream_boundary_live` (`mod.rs` 6175) read the gate.
- `blt_async_facts` (`virtio/gpu/blt_async.rs` 75-94) routes the Blt Direct when the boundary is ready. When it is not, the Blt is
  Deferred into the WindowedBlt FIFO (`queue_async_blt`, 255).
- The HPD worker later submits it (`ddi/display.rs` 1880 `service_windowed_blt` -> `virtio/venus/present.rs` 1599-1617 ->
  `ctrl.rs` 2656-2676). That worker hop is the 0.61 ms.

**The gap.** A GPU ACQUIRE needs a GPU virtual address in the CE channel's VA space and a payload. The KMD sees neither: the
fence handle is opaque, and the semaphore memory belongs to NVK's client and VA space.

### 2.2 What the push buffer needs

Use host-class (`*6F`) methods for both the acquire and the release, as NVK does on every channel (0035:20-21). They are executed
by the channel's PBDMA ahead of any engine, so they work on a CE runlist without depending on the CE class's semaphore fields.
Encoding from 0002:1017-1031 and 0006:321-345:
- `SEM_ADDR_LO` (0x5c), `SEM_ADDR_HI` (0x60), `SEM_PAYLOAD_LO` (0x64), `SEM_PAYLOAD_HI` (0x68), `SEM_EXECUTE` (0x6c), one
  incrementing method header of count 5.
- `SEM_EXECUTE` fields: `OPERATION` 2:0 (`RELEASE` = 1, `ACQ_STRICT_GEQ` = 2), `ACQUIRE_SWITCH_TSG` 12, `RELEASE_WFI` 20,
  `PAYLOAD_SIZE` 24 (`64BIT` = 1).

The per-Present push buffer:
1. **Acquire** the producer's value: `ACQ_STRICT_GEQ | ACQUIRE_SWITCH_TSG_EN | PAYLOAD_SIZE_64BIT`. The 64-bit compare is unsigned
   "current >= payload", not circular. `SWITCH_TSG` lets the runlist schedule other work while the acquire is pending.
2. **CE copy**: the class's `OFFSET_IN/OUT`, `PITCH_IN/OUT`, `LINE_LENGTH_IN`, `LINE_COUNT`, the source block-linear parameters
   (`SET_SRC_BLOCK_SIZE`, `SET_SRC_WIDTH/HEIGHT/DEPTH/LAYER`, `SET_SRC_ORIGIN`) when the image is block-linear, then `LAUNCH_DMA`
   with `DATA_TRANSFER_TYPE_NON_PIPELINED`, `FLUSH_ENABLE`, `SRC/DST_TYPE_VIRTUAL`, `MULTI_LINE_ENABLE`, the layouts, and
   `SEMAPHORE_TYPE_NONE`. Method offsets and field positions come from Mesa's `clc7b5.h` / `clcab5.h` (the Mesa tree
   `build-windows.sh` builds; this repo vendors no CE header). NVK's own copy code (`nvk_cmd_copy.c`) is the working model for the
   block-linear fields.
3. **Release** the KMD's completion semaphore: host `SEM_EXECUTE` `RELEASE | RELEASE_WFI_EN | PAYLOAD_SIZE_64BIT` (the WFI waits for
   the CE to go idle), followed by `NON_STALL_INTERRUPT` when an event is wanted (4.3).

The CE class's own release (`SET_SEMAPHORE_A/B/PAYLOAD` with `LAUNCH_DMA.SEMAPHORE_TYPE` one-word or four-word with timestamp) is
an alternative that saves the WFI. Its 64-bit payload support on 0xcab5 is unknown here (verify in `clcab5.h`). Timestamps: the host
`SEM_EXECUTE.RELEASE_TIMESTAMP` writes a 16-byte release with the GPU time. The repo never uses it (field position per `clc56f.h`),
and the prototype uses it only for measurement (section 5).

### 2.3 How the KMD would learn the producer's address and payload

Two routes, neither built:
- **(a) Record at context creation.** `SEMSURF_FENCE_CTX_CREATE` already passes through the KMD as a FORWARD, and its nested
  `hClient` is checked there (`nvrm-escape.md` 1040). The KMD can record `(device, ctx) -> (hClient, hSemaphoreSurface, index)`
  and map a later `SEMSURF_FENCE_CREATE` `{ctx, wait_value}` (`nvrm_fence.rs` 53-64) to an address. What remains unknown: which
  memory object backs `hSemaphoreSurface` (the alloc params are `{hSemaphoreMem, hMaxSubmittedMem, flags}`, 0030:1321-1328, and
  pass through the KMD only at the surface's `RM_ALLOC`).
- **(b) Extend the tail (preferred).** Add a fourth `HeliosRmFenceTail` variant (or HEPR/HERF v3) with `{hClient, hSemaphoreMem,
  offset, value}` that NVK fills from what it already has. The KMD validates `hClient` against the presenting process's own
  recorded clients, the same rule `NvDupHarden` applies (`shared-foreign-surfaces.md` 306-307).

With either route, the KMD's client dups the semaphore memory (`NV_ESC_RM_DUP_OBJECT`, `hClientSrc = NVK's client`) once per
timeline and maps it into its own VA space (`MAP_MEMORY_DMA`, snooped 4 KiB system PTEs). The acquire address is that VA plus
`offset`, and the payload is `value`. The host backend is one process, so host RM sees every guest client as the same process. That
is why `crm_share_smoke` can dup across guest processes. This is an inference, verified only for memory objects. The source image
needs the same dup: the foreign table holds `rm_handle` but not the creator's client (`kmd_logic/src/foreign_resource.rs` 669-681).
The KMD keeps the old opaque handle as well, so the Venus fallback and the DMA-fence gate are unchanged.

## 3. Destination pages as RM memory

The host side exists. The three ways to allocate an OS descriptor, the guest-physical page runs (direct or `PAGE_RUNS_INDIRECT`) and
the stitched host alias (one `PROT_NONE` span, `MAP_SHARED | MAP_FIXED` per run from the guest-RAM fd) are in
`host/backend/device/src/nvidia/osdesc.rs` 63-298 and `guestmem.rs` 132-246. That alias replaces the guest address before RM sees
it, and host RM pins it. Limits:
- whole 4 KiB pages, with the runs summing to `limit + 1`;
- each run inside one RAM region;
- at most `MAX_RUNS_INDIRECT` (about 262k) runs and 64 GiB.

The span is released when RM frees the object or its client, or when the file closes (`osdesc.rs` 300-361). A parent-device free
does not release it (`osdesc.rs` 333-338). On the guest side the KMD already PINs and names page runs for user mode
(`nvrm-escape.md` 4.4, 245-292), and `crm_pin_smoke` passes through it.

Status: `kmd-rm-client.md` 5.3 row B (231) is **UNVERIFIED** only for NVKMS GEM import and export of an OS descriptor, that is, for
display. This route never shows the destination on a display; it only GPU-maps it. That is what `crm_pin_smoke` does at 512 MiB.
No GPU *write* into an OS descriptor over guest pages has ever been checked: the prototype's job.

GPU mapping: the KMD's VA space, an `NV50_MEMORY_VIRTUAL` per mapping (`guest/rmclient/README.md` "GPU mappings"), with
`MAP_MEMORY_DMA` and `PAGE_SIZE_4KB | CACHE_SNOOP_ENABLE` as NVK maps system memory (`guest/nvk-rm/README.md` 1092-1100). Not
`GPU_CACHEABLE_YES` (0027:174-190): that needs an L2 sysmem invalidate per exec, and a write-only CE target gains nothing from it.

Coherence: the pages are guest RAM, and the host alias of them is ordinary cacheable memory. Snooped CE writes are coherent with the
CPU caches that dxgkrnl's and DWM's CPU view uses. This is the same argument as GuestBlob (`docs/VENUS.md` "Guest-memory blobs",
measured 0 bad pixels through a separate mapping); the RM route itself is unverified.

Lifetime: the guest-blob contract carries over one to one (`zero-copy-present.md` 24.12.2):
- one record per destination, created lazily on the first covered Blt;
- the lease set pinned for as long as the RM object lives;
- retired on eviction (`Draining -> Gone`).

The order of retirement differs:
1. Stop new copies.
2. Wait until the KMD completion semaphore reaches the last submitted copy's value. The GPU must be done, because RM's pin does not
   protect pages the guest re-uses.
3. `RM_FREE` the `NV50_MEMORY_VIRTUAL` mapping and the descriptor. The host munmaps the span.
4. Release the guest pin.

The bounded waits (250 ms per phase) and the StopDevice retire of v343 apply unchanged. Host constraint (g) of
`kmd-handoff-2026-10.md` 4 (one backing file per blob) has an equivalent here, "one RAM region per run" (`guestmem.rs` 217-222),
which is looser.

## 4. Submission and completion

### 4.1 Where the channel's memory lives, and how the KMD writes it

- **USERD and error notifier**: 8 KiB of `NV01_MEMORY_SYSTEM`, CPU-mapped through the RM window as the KMD already does for level 2+
  (`rm_client.rs` 1163-1284). `GP_PUT` is one 32-bit store at +0x8c.
- **GPFIFO ring and push buffer**: KMD nonpaged pages registered as an OS descriptor (3), GPU-mapped. The CPU writes them directly,
  with no window space used. Size: 64 entries x 8 B plus 64 push slots of 512 B. A copy with block-linear setup is about 40 dwords,
  and one Present needs one entry.
- **Progress**: `GP_GET` is not written back (0009:9-15), so ring space is read from the completion semaphore's sequence number,
  as NVK does.

### 4.2 Doorbell

A write of the work-submit token to `usermode + 0x90`. The host maps the usermode object into the shared window (region 1) and the
guest's store reaches the GPU MMIO with no proxy (`nvidia/rm_fd.rs` 150-300, `nvidia/window.rs` 96-104). The KMD maps region 1 the
same way (`kmd_logic/src/rm_window.rs` 1-15). Nothing in the backend checks who writes it. Whether the store causes a VM exit (it
lands in a BAR-backed window) is unknown: the prototype measures it. The Present DDI runs at PASSIVE, so per frame the KMD writes
memory, issues one fence, and makes two MMIO stores with no RM call.

### 4.3 Completion into the Present's DMA fence

Today's machinery is generic: the per-submission `WddmPending` record (`mod.rs` 2036, `note_wddm_submission` 7737) carries a
`stream_boundary`. `take_one_ready_wddm` (`mod.rs` 8179) waits for every boundary before `signal_dma_completed`
(`submit_command.rs` 883). The CE copy only has to become one more boundary. Options:

| option | per-frame RM calls | delivery | notes |
|---|---|---|---|
| A. KMD semaphore surface + `SEMSURF_FENCE_CREATE` per Present, attached through the existing RM gate | 1 (60-80 us, `guest/nvk-rm/README.md` 805) | host fence pump -> `EventReady` -> `nvrm_events` DPC | reuses `rm_gates.rs` as is; a GPU-written value fires "within ~0.1 ms of the write" (README 811) |
| B. one KMD-owned `NV01_EVENT_OS_EVENT` (FIFO non-stall, `SET_NOTIFICATION` REPEAT) and the DPC reads the KMD's own semaphore value | 0 | `NON_STALL_INTERRUPT` -> host event pump -> `EventReady` -> DPC compares the value | new: the KMD client allocating an OS event; edge-triggered, 1 ms sweep (`conduit-backend.rs` 768-860) |
| C. poll the value from the DPC and the heartbeat | 0 | the next interrupt or tick | fallback only |

Recommendation: B for M3 (no per-frame call, and one event covers every Present), A in the prototype because it already works end to
end. Measured references:
- `crm_semsurf_smoke` (CPU `SET_VALUE`): fence create 62 us, `SET_VALUE` to event 932 us median. A CPU write raises no interrupt,
  so this is the pump's sweep.
- `vk_rmfence_test` (GPU release): the RM fence woke no later than `vkWaitForFences` (`guest/nvk-rm/README.md` 815-821).
- The RM call itself costs about 55 us (`kmd-handoff-2026-10.md` 83).

### 4.4 Latency budget (projection; only the copy is measured)

| stage | today | CE channel |
|---|---|---|
| Present DDI | 0.135 ms (`PrDdiBltUs`) | similar plus about 2 us of ring writes |
| producer wait | 0.61 ms in a KMD worker (`BltDeferUs`) | inside the GPU acquire, overlapped with nothing else on the CE runlist |
| submit to copy start | part of the 0.3-0.8 ms host ring-1 overhead | doorbell MMIO, unknown (expected in the order of 10 us) |
| copy (1600x900 BGRA) | 0.2 ms | about 0.2 ms (the CE at about 28 GB/s; the Venus copy used `vkCmdCopyImageToBuffer`, `docs/VENUS.md`) |
| completion to DMA-fence retire | inside the 0.5-1 ms round trip | about 0.1 ms GPU-write-to-`EventReady` plus the DPC (unknown on this path) |
| total after the producer finishes | about 0.6 + (0.5-1) ms | about 0.3-0.4 ms (target) |

## 5. User-mode prototype (M1): `crm_ce_copy_smoke`

New file `guest/rmclient/tests/crm_ce_copy_smoke.c`, next to `crm_semsurf_smoke.c`. Extending that smoke instead would mix a host
fence test with channel code, so a sibling is cleaner, and it borrows semsurf's surface, fence and statistics code. It is Windows
only (exit 77 elsewhere) and about 700 lines of C. Base files: `crm_semsurf_smoke.c` (surface, fence, latency stats, PASS/FAIL
style), `crm_pin_smoke.c` (OS descriptor over `crm_alloc_pages`), `crm_smoke.c` 164-191 (usermode map through the subdevice),
`crm_share_smoke.c` (cross-client dup). The channel sequence is ported from patch 0006/0008/0035 (section 1.1).

**As built** (`guest/rmclient/tests/crm_ce_copy_smoke.c`; run lines, output and what to send back in
`crm_ce_copy_smoke.md` next to it). Where it differs from the plan below:
- It builds and runs on Linux too. Only `--fence` (doorbell to RM fence event) is Windows-only.
- The CE method defines are copied into the test, each cited to its header. No Mesa include path is needed.
- GPU time: CE semaphore releases with timestamp before and after the copy (`copy_us`), instead of a host release
  timestamp.
- The completion value is in RM system memory. A semaphore surface goes over it only with `--fence`. The timestamps and the
  probe are in OS-descriptor pages.
- The producer value is a CPU store by default (`--release semsurf` uses `SET_VALUE`).
- The ring has 128 entries. Every channel uses the device's VA space (`--vas new` tries a new one) at fixed VAs below 2^40.
- Block-linear: `--bl-roundtrip` / `--bl-probe-pitch` (M1b, section 10.4). Not built: a compressed source (unknown 1 of
  section 6).
- Added: `--engine`, `--contend`, `--delay`/`--duration` (section 7) and the `precondition:` line (section 8).

Structure (two clients in one process, so the cross-client route of 2.3 is exercised):
1. `producer` client: device, subdevice, 4 KiB `NV01_MEMORY_SYSTEM` (CPU-mapped) plus an `NV_SEMAPHORE_SURFACE` over it, the
   "producer timeline", and a 1600x900 BGRA source as `NV01_MEMORY_LOCAL_USER`, filled with a pattern through a BAR1
   `crm_map_memory`. Pitch layout first; a block-linear source is the `bl` argument.
2. `copier` client (stands in for the KMD):
   - device (`OPTIONAL_MULTIPLE_VASPACES`), subdevice and `FERMI_VASPACE_A`;
   - `GPU_GET_ENGINES_V2` and `CE_GET_CAPS_V2` to pick a non-GR async CE;
   - TSG `{engineType = COPY(n)}`, subcontext, channel (64 entries), BIND, CE object `{engineType}`, token, schedule;
   - usermode mapped through the subdevice.
3. `crm_dup_object` of the producer's semaphore memory and source into `copier`, each mapped with `crm_map_dma2`.
4. Destination: `crm_alloc_pages` (ordinary process pages, as the KMD's lease pages would be), registered with
   `crm_alloc_os_descriptor`, mapped snooped. A KMD completion semaphore lives in a second semaphore surface (option A) and in
   OS-descriptor pages (polled).
5. Rounds (default 200), each with a fresh producer value V and completion value C:
   - **ready**: the CPU sets producer >= V (`SET_VALUE` 0xda0004), then submits [acquire V; copy; release C with timestamp].
     Measures doorbell to C seen (spin on the CPU mapping) and doorbell to fence `EventReady`.
   - **wait**: submit first; after 2 ms the CPU sets V. It checks that C has NOT landed before the set (the acquire held) and
     measures set to C seen.
   - **doorbell**: a push of only [release S with timestamp], no WFI. Measures doorbell to S seen. This stands in for GPFIFO
     `GP_GET`, which USERD does not report.
   - Every 16th round, the destination is compared with the pattern (0 bad bytes).

Allowlist needs: only the rows in section 1.2, all present in 610.57.04. `RM_DUP_OBJECT`, `ALLOC_MEMORY` with page runs and
`MAP_MEMORY_DMA` are not table-gated.

Meson (`guest/rmclient/meson.build`, after `crm_share_smoke`):

```meson
# A copy-engine channel: semaphore acquire on another client's semaphore, CE copy
# into an OS descriptor over process pages, release; exits 77 elsewhere.
executable('crm_ce_copy_smoke', 'tests/crm_ce_copy_smoke.c',
  include_directories : [inc, include_directories('src')],
  dependencies : rmclient_dep)
```

Build on the host (MinGW cross, release, as `guest/nvk-rm/build-windows.sh` 117-122 builds librmclient). The CE method header
comes from a Mesa checkout (`-Dc_args=-I<mesa>/src/nouveau/nvidia-headers`) or a copy of the few defines into the test file:

```sh
cd guest/rmclient
meson setup build-win --cross-file ../nvk-rm/windows/mingw-x86_64.ini -Dbuildtype=release
ninja -C build-win crm_ce_copy_smoke.exe librmclient.dll
x86_64-w64-mingw32-strip build-win/crm_ce_copy_smoke.exe build-win/librmclient.dll
```

Run (main session; copy both files into the guest's public tools folder; `$WIN_SSH` as in `guest/windows/ci/vm/win-build.sh`):

```sh
scp -P 2222 build-win/crm_ce_copy_smoke.exe build-win/librmclient.dll "$WIN_SSH:C:/Users/Public/t/"
ssh -p 2222 "$WIN_SSH" 'C:\Users\Public\t\crm_ce_copy_smoke.exe --iterations 200'
```

Expected output as planned (the built tool prints a stage table instead; see `crm_ce_copy_smoke.md`):

```
[ ok ] producer client: semaphore surface + 1600x900 source (pitch)
[ ok ] copier: CE engine COPY2 class 0xcab5, channel 0xca6f, token 0x...
[ ok ] dup + GPU map: producer semaphore, source; destination OS descriptor 1407 pages
[ ok ] ready: doorbell->done p50 ... us p99 ... us; doorbell->EventReady p50 ... us
[ ok ] wait: acquire held 200/200; release->done p50 ... us p99 ... us
[ ok ] doorbell->first release p50 ... us p99 ... us
[ ok ] pattern: 13 checks, 0 bad bytes
CE COPY SMOKE: PASS
```

Pass criteria:
- every step OK;
- in "wait", no round where C landed before the CPU set V;
- 0 bad bytes;
- the error notifier stays 0;
- `crm_object_count` is 0 after teardown;
- `NvPin == NvUnpin` afterwards (`nvrm-escape.md` 706-709).

Numbers that decide M3: ready doorbell->done p50 below 400 us (the copy is about 200 us), and wait release->done below 50 us plus the
copy.

## 6. Missing pieces, in dependency order

| # | piece | owner | size | depends on |
|---|---|---|---|---|
| 1 | `crm_ce_copy_smoke` (M1) and a test request to the main session | this lane | about 700 lines of C, 1 session | nothing |
| 2 | host fixes only if M1 finds a refusal (none expected from the tables; possibly the OS-descriptor GPU-write path or event delivery for a GPU release) | host session | 0-0.5 session | 1 |
| 3 | the producer address for a Present: the tail variant `{hClient, hSemaphoreMem, offset, value}` (2.3 b), protocol ABI plus an NVK patch | NVK session (plus protocol in this lane) | about 150 lines, 1 session | 1 |
| 4 | KMD RM client channel subsystem: VA space, GPU map steps, TSG/channel/CE/token/schedule, USERD and ring memory, doorbell map, teardown and device-loss handling | this lane | about 1500 lines of Rust, 2-3 sessions | 1 |
| 5 | KMD destination descriptors: OS descriptor from the lease runs, per-destination record on the GuestBlob lifecycle, plus the source and semaphore dup/map cache keyed by foreign resource | this lane | about 1000 lines, 2 sessions | 3, 4 |
| 6 | completion: KMD OS event (option B) or semaphore surface plus fence (A), a new boundary kind in `WddmPending` | this lane | about 400 lines, 1 session | 4 |
| 7 | routing: a new route in `ba::decide` behind a knob (`BltRmCe`, default 0), the Venus copy as fallback on any refusal or strike, counters (`Ce*`) | this lane | about 400 lines, 1 session | 5, 6 |

Top unknowns, in order of risk:
1. **The source image through the copier's VA space.** Can the KMD client dup NVK's image memory and map it with the right PTE
   kind? The source can be block-linear (patch 0024) and on GB20x compressible (patch 0028); the CE reads compressed data correctly
   only through a mapping with the compressible kind. Verify in M1 with `bl` and a compressed source; fallback: pitch-only, else
   Venus.
2. **Completion delivery for a GPU release on a KMD-owned path.** Is it about 0.1 ms (README 811) or the pump's 1 ms sweep? M1
   measures doorbell->`EventReady`.
3. **A GPU write into an OS descriptor over guest pages.** `crm_pin_smoke` only maps one. Also whether a device allocated without
   `OPTIONAL_MULTIPLE_VASPACES` (the KMD's today) can take its own `FERMI_VASPACE_A` for a CE-only channel. M1 checks both.

Smaller unknowns:
- The doorbell's VM-exit cost.
- The 64-bit payload of the CE's own semaphore release on 0xcab5.
- Whether dxgkrnl ever touches the destination while a CE copy is in flight. The DMA fence orders this today; it is unchanged as
  long as the fence retires on the completion semaphore.

## 7. Concurrency with the 3D channel, and the PCIe stage line

### 7.1 Which engine a copy runs on

A channel's engine is fixed when it is allocated. The same `engineType` (an `NV2080_ENGINE_TYPE_*` value) goes into three
places:
- the TSG's `NV_CHANNEL_GROUP_ALLOCATION_PARAMETERS.engineType`;
- the channel's `NV_CHANNEL_ALLOC_PARAMS.engineType`;
- `NVA06F_CTRL_CMD_BIND {engineType}`.

RM puts the TSG on that engine's runlist. NVK-on-RM shows both cases:
- **Its 3D channel** uses `NV2080_ENGINE_TYPE_GRAPHICS` (0006:598-638, BIND in 0008:596-611). It allocates the copy object
  with NULL parameters, and "RM picks the GR copy engine" (0006:529-532). That copy engine is the graphics engine's CE
  (GRCE). It runs on the GR runlist, inside the 3D channel's TSG.
- **Its video channel** uses `NV2080_ENGINE_TYPE_NVDEC0` in all three places (0035:68-97) and gets a runlist of its own
  ("NVDEC runs on a runlist of its own", 0035:68).

A CE channel is the second case with `NV2080_ENGINE_TYPE_COPY(n)`. Values from the 610.57.04 open kernel modules'
`cl2080_notification.h`: `COPY0..COPY9` are 0x09..0x12, `COPY10..COPY19` are 0x34..0x3d, and
`COPY(i) = i < 10 ? 0x09 + i : 0x34 + i - 10`. The CE object then takes `NVB0B5_ALLOCATION_PARAMETERS {version =
VERSION_1, engineType = COPY(n)}`. With `VERSION_1`, engineType is an `NV2080_ENGINE_TYPE` (`clb0b5sw.h`; VERSION_0 would read
it as a CE instance number).

**GRCE and async CEs.** The difference follows from the runlists:
- A copy on the GRCE (the copy object on the 3D channel, or a separate TSG on the GR engine) is on the GR runlist. It
  time-slices with the app's 3D TSG: a copy submitted while the app renders waits for a TSG switch, and the app waits
  for it in turn.
- An async CE has its own runlist (one per CE engine, as for NVDEC). Its TSG is scheduled independently of GR, so the copy
  runs while the app's 3D work runs. The two compete only for memory and PCIe bandwidth.

**UNVERIFIED** (no repo file shows it): that GSP-RM accepts a TSG with `engineType = COPY(n)` from an unprivileged client.
The allowlist gates only class and control, so nothing on the host refuses it (1.2), but no tool has allocated one yet.
Also unverified: that every non-GRCE CE has its own runlist on GB202 and Ada. The prototype answers both (7.3).

### 7.2 Which copy engines exist, and how a client finds an async one

The repo has no per-GPU CE count. `docs/GPU-SUPPORT.md` 27 lists only the copy class per generation (Ada 0xc7b5, Blackwell
0xcab5), `nvgpu_rmalloc_classes.h` lists only the parameter sizes (8 B for every `*_DMA_COPY_*`), and the allowlist rows are
in 1.2. **UNVERIFIED:** how many `COPYn` GB202 and AD10x expose, and which of them are GRCEs. The prototype prints both.

Every query a client needs is allowed in 610.57.04 (`v610_57_04.rs` line in brackets):
- `NV2080_CTRL_CMD_GPU_GET_ENGINES_V2` 0x20800170, 340 B (268): `{engineCount, engineList[0x54]}` of `NV2080_ENGINE_TYPE`
  values. The `COPYn` entries are the CEs this GPU exposes.
- `NV2080_CTRL_CMD_CE_GET_CAPS_V2` 0x20802a03, 8 B (470): `{ceEngineType, capsTbl[2]}` for one CE. In byte 0, bit 0x01 is
  `CE_GRCE`, 0x02 is `CE_SHARED` and 0x08 is `CE_SYSMEM_WRITE`. In byte 1, 0x01 is `SUPPORTS_NONPIPELINED_BL`
  (`ctrl2080ce.h`).
- `NV2080_CTRL_CMD_CE_GET_ALL_CAPS` 0x20802a0a, 136 B (471): the same caps for all 64 CE slots plus a `present` mask, in one
  call.

The rule for a KMD client: take the first `COPYn` from `GET_ENGINES_V2` whose caps lack `CE_GRCE` and have
`CE_SYSMEM_WRITE` (the destination is guest RAM). Prefer one without `CE_SHARED`, then use `COPY(n)` in the TSG, channel,
BIND and CE object. Neither `NV2080_CTRL_CMD_FIFO_GET_DEVICE_INFO_TABLE` (0x20801112) nor any other runlist query is in the
allowlist. The work-submit token carries the runlist, though:
- `RUNLIST_ID` 22:16 and the channel id in 11:0 (the 610.57.04 open kernel modules: `kfifoGenerateWorkSubmitTokenHal_GB202`
  with `dev_vm.h` `NV_VIRTUAL_FUNCTION_DOORBELL_*`; `kfifoGenerateWorkSubmitTokenHal_GA100` with `dev_ctrl.h`
  `NV_CTRL_VF_DOORBELL_*`);
- so two channels on different runlists show different token bits 22:16.

### 7.3 What the prototype does about it

`crm_ce_copy_smoke`:
- prints every `COPYn` with its caps and takes the first async one by default (`--engine <n>` picks one, `--engine gr`
  builds the channel on GR with a NULL-parameter copy object, as NVK does, for the A/B);
- prints the engine and the runlist id decoded from the token.

**UNVERIFIED:** whether GSP-RM accepts a GR channel with only a copy object and no 3D object. NVK always allocates 3D
first.

Concurrent mode (`--contend`): a GPU 3D load of our own cannot be built here, because user mode has no shader stack. Instead
a second channel (`--contend-engine`, default `gr`) runs a large copy, 64 MiB by default, from video memory into another
OS descriptor in guest RAM. That copy uses the same PCIe write path. The measured CE copy starts while it is in flight. The
tool then reports:
- `copy_us` (no competitor) next to `copy_us_contended`;
- `contend_overlapped`, the rounds in which the competitor was still running when the measured copy completed;
- whether the two channels got the same runlist, that is, whether they time-slice or run concurrently.

The real contention case is the second run with a windowed 3D app, using `--delay` to start it (`guest/rmclient/tests/crm_ce_copy_smoke.md`, run 3).

### 7.4 The PCIe stage line: today and with the CE route

Host measurement of today's windowed copy: 5.8 MB per frame written into guest RAM over PCIe while a 3D app shares the
GPU. Host dispatch to MSI is 446 us p50 / 828 us p99, and the guest sees 0.5-1 ms in total. The CE route keeps the
transfer and drops the hops around it. The producer wait moves into the GPU acquire, so the copy starts on the producer's
release and overlaps its tail instead of waiting for a KMD worker.

| stage | today (us) | expected with the CE route (us) | the prototype measures it with |
|---|---|---|---|
| producer wait (KMD worker) | 610 mean (`BltDeferUs`) | 0 on the CPU; inside the GPU acquire | `acquire_satisfy_to_done_us` minus `copy_us`: what remains after the release |
| submit (host dispatch, or doorbell to the GPU reading the push) | part of about 105 host CPU overhead (being cut by 50-60) | the doorbell MMIO, expected about 10 | `doorbell_to_gpfifo_get_us` |
| GPU copy plus the driver wake | about 345 | about 345 for the transfer, with no host driver wake | `copy_us` (GPU timestamps around the copy), and `copy_us_contended` |
| MSI injection | 3-6 | none for the copy itself | n/a |
| guest kick exit, MSI -> ISR -> DPC, delivery | 100-400 | the completion path of 4.3 (about 100 for a GPU release, unverified) | `doorbell_to_event_us` minus `submit_to_done_acquire_satisfied_us` (`--fence`) |
| total after the producer finishes | 500-1000 (guest view) | about 345 plus the completion path | `acquire_satisfy_to_done_us` (the copy overlaps the producer's tail) |

The ~150 us of host hops (the ~105 us CPU overhead plus the wake and delivery around the transfer) is what the route removes.
The ~345 us of transfer stays unless the copy gets faster on an idle async CE. `copy_us` against `copy_us_contended` shows
how much PCIe contention costs.

## 8. Precondition for M2: how guest RAM is backed

The destination is guest RAM, so the copy's cost depends on how the host backs those pages:
- At the time of writing, guest RAM is memfd/shmem with transparent huge pages in `within_size` mode, not reserved
  hugepages.
- `nr_hugepages=8192` (2 MiB pages) is staged for the next host reboot. No 1 GiB pages are configured.
- The GPU reaches guest RAM through the host IOMMU/driver mapping of those pages. The OS descriptor's runs become one
  stitched host alias that RM pins (section 3).

Every M2 result records this backing state next to its numbers: THP shmem or reserved 2 MiB hugepages, and the
`nr_hugepages` in effect. The tool prints `precondition: copy_bytes=<n> pages=<n>` (the 4 KiB pages the destination
touches; 1407 for 1600x900) so that the page count is on record with each run. If the per-page cost looks high (`copy_us`
well above 5.76 MB at the CE's bandwidth), a later A/B with reserved 2 MiB hugepages separates the page-mapping cost from the
transfer.

## 9. Milestones

- **M0**: this document.
- **M1**: `crm_ce_copy_smoke.c` plus the meson entry (section 5), and a test request to the main session with the exact build and
  run lines.
- **M2**: numbers from M1 on GB202 (and Ada if available): ready, wait, doorbell latencies and the pattern check. Go or no-go on the
  section 5 thresholds; unknowns 1-3 answered.
- **M3**: KMD integration (pieces 3-7) behind `BltRmCe`, with the Venus GuestBlob copy as fallback, then the GuestBlob sign-off
  procedure of `kmd-handoff-2026-10.md` 4 repeated for this route.

## 10. Fence tail v3: what NVK must fill

Route (b) of 2.3, built in M3a: an optional 96-byte record behind the 16-byte fence tail. ABI and parser:
`protocol/src/rm_fence_v3.rs`; C mirror: `protocol/include/helios_rm_fence.h` (pinned by a protocol test, compiled as C 64/32-bit
and C++). Nothing existing changes size or meaning: the fence tail keeps its rules (`rm-fence-marker.md`), `rm_fence_value`
in it stays diagnostic, and a KMD or producer that does not know the record works as before.

### 10.1 Wire shape

```text
HERF, CommandLength = 168            HEPR, CommandLength = 192
   0..32   HeliosPresentRefreshCmd      0..80   HeliosPresentRenderCmd (reserved |= FLAG_RM_FENCE)
  32..48   HeliosRmFenceTail (FENCE)   80..96   HeliosRmFenceTail (FENCE)
  48..72   on-scanout slot, ALL ZERO   96..192  HeliosRmFenceTailV3
  72..168  HeliosRmFenceTailV3
```

| offset | field | type | rule |
|---|---|---|---|
| 0 | `magic` | u32 | `'HEF3'` (0x33464548); zero = no record |
| 4 | `version` | u16 | 3; any other version is refused, never reinterpreted |
| 6 | `flags` | u16 | `SEMAPHORE` (1) and `SOURCE` (2), both required |
| 8 | `bytes` | u32 | at least 96 and inside the command (a later revision may be longer) |
| 12 | `reserved` | u32 | zero |
| 16 | `semaphore.h_client` | u32 | the producer's RM client; nonzero |
| 20 | `semaphore.h_memory` | u32 | `hSemaphoreMem` of the timeline's `NV_SEMAPHORE_SURFACE`; nonzero |
| 24 | `semaphore.offset` | u64 | byte offset of the 64-bit value; 8-aligned, no overflow |
| 32 | `semaphore.value` | u64 | the value the frame's work releases; nonzero; must equal `rm_fence_value` |
| 40 | `source.h_client` | u32 | the producer's RM client; nonzero |
| 44 | `source.h_memory` | u32 | the presented image's RM memory; nonzero |
| 48 | `source.offset` | u64 | plane 0 offset in that memory |
| 56 | `source.size` | u64 | bytes of the memory object |
| 64 | `source.modifier` | u64 | `DRM_FORMAT_MOD_LINEAR` or `gb20x_family(bpp) \| h`, `h <= 5` |
| 72 | `source.pitch` | u32 | row pitch in bytes; at least the row, aligned, at most 1 MiB; block-linear: a multiple of 64 |
| 76 | `source.width` | u32 | 1..16384 |
| 80 | `source.height` | u32 | 1..16384 |
| 84 | `source.fourcc` | u32 | a one-plane format of `share_format` |
| 88 | `source.flags` | u32 | `COMPRESSED` (1) known (the route refuses it); other bits refused |
| 92 | `source.reserved` | u32 | zero |

`offset + pitch * rows <= size`, with `rows` the height rounded up to the block (`8 << h` rows) for block-linear, in checked
arithmetic. Why a record and not a bumped `HERF`/`HEPR`: a bumped version would be ignored whole by an older KMD, losing the
refresh arm and the fence (the reason `rm-fence-marker.md` gives for not bumping them). The on-scanout slot stays zero: an
on-scanout frame is never copied, so the tag and the record never meet, and the zero slot parses as "no tag" in every KMD.

### 10.2 Where NVK knows each value

The values come from two places in NVK's present path (`patches-windows/0030`, with `helios_image_layout` of 0025/0041):

| field | NVK source |
|---|---|
| `semaphore.h_client` | `dev->lib->root(dev->client)`: the client `rmfence_tl_locked` already passes to `SEMSURF_FENCE_CTX_CREATE` |
| `semaphore.h_memory` | `nvkmd_rm_mem(rf->mem)->h_memory`: the `hSemaphoreMem` of `rmfence_init`'s surface |
| `semaphore.offset` | `index * rf->entry_size_B`: `offset_B` of the queue's timeline in `rmfence_tl_locked` |
| `semaphore.value` | `++tl->next_value` of `nvkmd_rm_rmfence_sync`, the same value `queue_rm_fence` returns as `*value` |
| `source.h_client` | the same root client (the image is NVK's device memory) |
| `source.h_memory` | `nvkmd_rm_mem(mem->mem)->h_memory` of the presented image's dedicated memory |
| `source.offset` | `fl.offset` of `helios_image_layout` (`plane_offset_B + level offset_B`) |
| `source.size` | `mem->mem->size_B` |
| `source.modifier`, `pitch`, `width`, `height`, `fourcc` | `fl.modifier`, `fl.stride`, `fl.width`, `fl.height`, `fl.fourcc` of `helios_image_layout` |
| `source.flags` | `COMPRESSED` when `image->is_compressed` (better: send no record then) |

`helios_scanout_present_fenced` already computes all the source values for the scanout path. The windowed path does not:
the UMD calls `nvk_present_fence` (`umd/src/bridge.rs`, DXVK bridge) -> `queue_rm_fence` (helios_icd_interface v3), which
returns only `(handle, value)` and knows no image. The NVK session's work:
1. A new appended `helios_icd_interface` entry (the next free version), for example
   `queue_rm_fence_v3(VkDevice, VkQueue, VkDeviceMemory, VkImage, uint32_t *fence_handle, struct HeliosRmFenceTailV3 *out)`:
   the fence as `queue_rm_fence` makes it, plus the record filled from the table above. `VK_ERROR_FORMAT_NOT_SUPPORTED` for a
   source `helios_image_layout` refuses (the fence is still returned; the UMD then sends the 48-byte tail).
2. The DXVK bridge passes the presented resource's memory and image (as `nvk_scanout_present_fenced` already does).
3. The UMD (`umd/src/forward/present.rs`, `MarkerPresent`) writes the 168-byte `HERF` when the KMD advertises the route
   (a new `QUERY_CAPS` bit in `supported_ops` bits 32..63, to be assigned in M3c), and the 48-byte one otherwise.

### 10.3 The block-linear case (the measured one)

Heaven's windowed source is block-linear: the UMD log shows `1600x900 stride=6400 offset=0 fourcc=0x34324241 (AB24)
modifier=0x0300000000606014 size=6553600`. Decoded as `DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(c=0, s=1, g=2, k=0x06, h=4)`:
blocks of 16 GOBs (128 rows), page kind 0x06 (generic), the GB20x GOB generation, sector layout 1, no compression. It is
`MOD_NVIDIA_BL_GB20X | 4`, the family `helios_image_layout` emits, so the record carries it unchanged. The 6553600-byte
object is exactly `6400 * 1024` (900 rows rounded up to 8 blocks); the copy reads 900 rows, 5760000 bytes. The protocol test
`the_heaven_source_is_a_valid_record_with_the_documented_bytes` pins this record.

**Open item: the mapping kind.** The KMD client dups `source.h_memory` and maps it in its own VA space. The copy engine
de-swizzles a block-linear source according to the GOB layout, and the PTE kind of the mapping decides the physical
swizzle within a GOB. A mapping with another kind (for example the pitch kind) reads scrambled pixels without any error.
The kind is therefore part of the dup+map contract: the KMD maps with the kind the modifier names (`k = 0x06`), as NVK maps
the image. Nothing checks this yet. Needed:
- **M1b**: a round trip in `crm_ce_copy_smoke`: copy a pitch pattern into a block-linear destination and back through the
  same mapping, then compare;
- **M3c**: a check against a real NVK image (Heaven windowed, the pattern compared with the Venus copy of the same frame).

The block-linear push-buffer words (`kmd_logic::ce_present`) are host-tested only; nothing claims block-linear works on
hardware.

### 10.4 M1b: the block-linear round trip in `crm_ce_copy_smoke` (built, not yet run)

`--bl-roundtrip` (run lines and expected output in `crm_ce_copy_smoke.md`) works as follows:
- The producer allocates a 6553600-byte image (pitch 6400 = 100 GOBs, 1024 rows = 8 blocks of 128) as plain video memory.
  As in NVK (patch 0004/0028 `alloc_rm_memory`), the allocation carries no kind.
- The copier dups it and maps it with big pages and the PTE kind given by `NVOS46_FLAGS_PAGE_KIND_OVERRIDE` (19:19) plus
  `NVOS46_PARAMETERS.kindOverride`. That is how patch 0005 (`nvkmd_rm_va_bind_mem`) gives an image's VA its kind, and
  0022/0027 keep the rule ("kinds are applied per mapping"). The default kind is the modifier's k, 0x06; `--bl-kind`
  overrides it.
- Per round, one push copies the salted pitch pattern into the image with the CE's destination block-linear state
  (`SET_DST_BLOCK_SIZE` 0x70c..`SET_DST_LAYER`, `DST_ORIGIN_X/Y` 0x74c/0x750, Mesa `clcab5.h`). It then makes a host release
  with WFI and copies the image back into an OS descriptor with the source block-linear state. Those words are the ones
  `ce_present::copy` emits for Heaven's source, checked against the builder's pinned test words. The CPU then compares
  every word.
- `--bl-probe-pitch` also copies the image out PITCH -> PITCH. Its position-dependent checksum must differ from the
  unswizzled pattern's.

What a PASS proves:
- that 0xcab5 accepts the block-linear methods at these offsets with these words: block-size word 0x1040,
  `SRC/DST_WIDTH` = pitch in bytes, `HEIGHT` = image rows (the method half of unverified item 2 of 11.7). A bad method or
  value raises an RC error on the tool's channel;
- that the pitch -> block-linear and block-linear -> pitch copies are exact inverses over the whole 1600x900 frame. A
  field that is wrong in the same way in both directions, such as the block height or `KIND_BPP`, still cancels out;
- that the CE really swizzles: the probe differs;
- that the copy rate from block-linear video memory into guest RAM is close to the pitch copy rate;
- that a dup of a video-memory image mapped with an override kind works from a second client.

What it cannot prove:
- **Compatibility with NVK's own layout and mapping.** Both directions go through the same mapping with the same block
  parameters, so any self-consistent but wrong choice cancels out. That includes the kind: `--bl-kind 0` is expected to pass the round
  trip as well. The checksum of the pitch probe under kind 0 versus 0x06 shows whether the kind changes the physical
  layout. A real NVK image rendered by the 3D engine, read through the KMD's mapping and compared with the Venus copy of
  the same frame, is the only check of 10.3's open item: M3c.
- A dup across processes (NVK's client in the app, the KMD's client), a compressed source, and the kernel-mode path. These
  stay unverified items 6, 7 and 8 of 11.7.

## 11. KMD integration points

M3a built the pure half: the record (10), and `kmd_logic/src/ce_present.rs` with the push-buffer builder (its pitch-linear
words reproduce `crm_ce_copy_smoke`'s, its block-linear words follow NVK's `nouveau_copy_rect`), the GPFIFO entry and ring
arithmetic, `source_plan` (the foreign layout rules of `foreign_resource::Layout`, the page kind to map with), the
per-destination `Route` (decision order, strikes, poison, timeout), the retire rule, the knob `RmCopyEngine` (default 0) and the
`Ce*` counter names. Sections 11.1 to 11.8 are the plan for the I/O half, written before it; line numbers are of v343. M3b
built the channel subsystem of 11.2 (without the source/semaphore dup cache and the destination descriptors, which are M3c's)
and a hardware self-test: what is built and how it differs from the plan and the tool is 11.9, the test procedure 11.10.

### 11.1 Where the route decision plugs in

The Present Blt arm (`ddi/display.rs`) today runs, for a foreign (NVK) source into a standard-buffer destination:
`guest_blob::prepare` (817-834) -> `blt_async::entry` (838-849) -> `blt_async::try_async` (945-957; `ddi/blt_async.rs`
507-560, which reads `blt_async_facts` of `virtio/gpu/blt_async.rs` 75-94 and takes DIRECT, DEFERRED or the legacy arm).

The copy-engine route goes FIRST inside the `entry.async_enter` branch, before `try_async`:
1. `ddi/ce_present.rs::try_ce(passive, adapter, args, source, destination, present_stream_boundary, record)` gathers
   `ce_present::Facts`: the knob, the channel's state, the stashed record of this context (below), `source_plan` of the
   record's source, `Route::admits` of the destination, the destination's coverage, ring room.
2. `ce_present::decide` -> `CopyEngine`: build `present_push` into the slot, `Ring::submit`, `kick` (the entry, `GP_PUT`, the
   doorbell), merge the copy-engine boundary (11.5) into the private record, `present_complete` as the DIRECT arm does.
3. `Venus { why, after }`: count `CeFallback`/`CeWhy`/`CeMask`; with `after = Some(v)` the boundary of the Venus copy must
   include "completion >= v" (merged into the private record like a second boundary, or the DEFERRED arm with that
   boundary), so a Venus frame is never overwritten by an older copy-engine frame. Then `try_async` runs unchanged.

Where the record comes from: `DxgkDdiRender` (`ddi/submit_command.rs`) already reads the fence tail of `HERF` (2275-2288)
and `HEPR` (2470-2485) and stashes the on-scanout tag per context (`ddi/onscanout.rs::note_render` 88-122). The record is
parsed there too (`HeliosRmFenceTailV3::parse` at offset 72 / 96, `matches_fence` against the tail just read, the
`h_client`s checked with `ClientTable::is_client_owned_by` for the NVRM devices of the context's `creator_process`), and
stashed on the context beside the on-scanout tag with the same pairing and orphan rule. The fence keeps its own path
(`attach_rm_fence_marker`, the RM gate); the record never changes what happens to the fence. The DEFERRED arm's worker
(`service_windowed_blt`, `ddi/display.rs` 1880) is not involved: the copy-engine route has no worker hop at all.

### 11.2 The channel subsystem in the KMD's RM client

The KMD's client (`virtio/rm_client.rs`, the step machine `perform` 782-846, `Io::rm_alloc` 758, the map steps
`rm_map_memory` 1163, `host_mmap` 1212, `kernel_map` 1249) grows, at a level of its own (`RmCopyEngine` 1 implies it):

| object | how | reference |
|---|---|---|
| device | the existing one; whether it needs `vaMode = OPTIONAL_MULTIPLE_VASPACES` is unverified (6, unknown 3); if so, a second device of the same client for the channel | `crm_ce_copy_smoke` copier |
| VA space | `FERMI_VASPACE_A` `index = GPU_DEVICE` | tool, nvk-rm 0008 |
| engine | `GPU_GET_ENGINES_V2` + `CE_GET_CAPS_V2`: the first async CE with `SYSMEM_WRITE`, preferring one without `SHARED` (7.2) | tool |
| TSG, subcontext, channel, BIND, CE object, token, schedule | 1.1 with `engineType = COPY(n)`, 64 or 128 entries | tool `chan_create` |
| USERD + error notifier | 8 KiB `NV01_MEMORY_SYSTEM` (`sysmem.rs` `alloc_sys` 715), CPU-mapped through the RM window (`rm_map_memory` -> `host_mmap` -> `kernel_map`) | `sysmem.rs`, `rm_client.rs` 1163-1284 |
| GPFIFO + push slots | 128 KiB: KMD nonpaged pages as an OS descriptor (the page-run block of `kmd_logic/src/page_runs.rs`, as `virtio/nvrm.rs` 793 `pin_pages` builds it for user mode), or RM sysmem through the window; GPU-mapped snooped below 2^40 | tool `osdesc_alloc` |
| doorbell | `*_USERMODE_A` under the subdevice (`{bBar1Mapping = 1, bPriv = 0}` from 0xc661 up), 64 KiB, mapped through the subdevice into window region 1 (`kmd_logic/src/rm_window.rs`) | `crm_smoke.c` 164-191 |
| completion | 4 KiB RM sysmem, CPU-mapped (window) and GPU-mapped; one value per channel, `Ring::submitted` | tool `done` |
| completion event | option B of 4.3: `NV01_EVENT_OS_EVENT` with `SET_NOTIFICATION` REPEAT; which notifier index a CE channel's `NON_STALL_INTERRUPT` raises is unverified (the tool measured only option A) | 4.3 |

The first push is `SET_OBJECT` + a release (`ce_present` test `the_first_push_reproduces_the_tool_words`); the channel is
"alive" when that value lands and the error notifier is 0. Teardown in the tool's order: schedule off, free the TSG (the
channel and the CE object with it), then unmap and free the memory, the doorbell, the VA space. Device loss and StopDevice
follow the RM client's existing `retire_begin` / `forget` (`rm_client.rs` 468-497): a lost transport frees nothing on the
host (the backend frees with the client), and every destination with copies outstanding is poisoned (`on_channel_failed`).

Per source and per producer semaphore (keyed by `(h_client, h_memory)`): `NV_ESC_RM_DUP_OBJECT` into the KMD client, then
`NV50_MEMORY_VIRTUAL` + `MAP_MEMORY_DMA`. The semaphore: 4 KiB, `PAGE_SIZE_4KB | CACHE_SNOOP_ENABLE`. The source: the page
kind of `SourcePlan::page_kind` (0x06 for block-linear; 10.3), the image's own page size. A small cache (8 sources and 4
timelines per process, LRU) because a swapchain rotates two or three images; entries are freed when the NVRM client that
owns the original is freed (`nvrm_clients::ClientTable::forget_client`) or the process ends, since a dup keeps the memory
alive on the host past the app's own free.

### 11.3 The destination: the GuestBlob lease and pin lifecycle

A destination is a KMD standard Present buffer whose system pages VidMm holds under `MmProbeAndLockPages` leases
(`adapter.system_backings`, `adapter/backing.rs` `guest_record` 451). The copy-engine route reuses that lifecycle
(`zero-copy-present.md` 24.12) one to one:
- **Create, lazily, on the first covered Blt**: when `guest_blob::build_runs` says the leases cover `[0, pitch * height)`, the
  KMD registers the same pages with its RM client as an `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` (`PAGE_RUNS_INDIRECT` for many
  runs, `osdesc.rs` limits) and GPU-maps it snooped. The record lives beside the guest-blob record (one more state in the same
  entry) so the eviction and destroy paths find both.
- **Pin**: the leases stay held while the descriptor exists (exactly the guest blob's rule).
- **Retire** (eviction in `BuildPagingBuffer`, destroy, StopDevice): (1) stop new copies (the record leaves `Ready`);
  (2) wait, bounded by the guest blob's 250 ms per phase, until the completion value reaches `Route::submitted`
  (`Route::teardown` -> `WaitFor`); (3) `RM_FREE` the mapping and the descriptor (the host unmaps its stitched alias);
  (4) the guest-blob retire, then the unlock. `Teardown::Leak` (a poisoned destination whose copy never completed) keeps the
  pages pinned for good, counted `CeLeak`, as `GbLeak` does.
- A destination with both a guest blob and a descriptor has two host aliases of the same pages; both are snooped, so the
  Venus fallback and the copy engine see the same memory. The `after` bound of 11.1 orders them.

### 11.4 Submission at Present (PASSIVE)

Per frame, no RM call: write about 34 dwords into the slot (KMD nonpaged memory), the 8-byte entry, `GP_PUT` (one store into
the window-mapped USERD), the token (one store into the window-mapped doorbell). The source and semaphore mappings and the
destination descriptor are made on first use (forwarded RM calls, about 55 us each, PASSIVE, outside every lock, under the
RM client's own serialization as `sysmem.rs` does); a frame that would need one takes the Venus copy while it is made.

### 11.5 Completion into the Present's DMA fence

The copy-engine boundary uses the existing tagged stream namespace, as the RM gates do (`rm-fence-marker.md`,
"Representation"): one KMD-owned "copy-engine gate" (a present-stream slot with no Venus context, `virtio/gpu/rm_gates.rs`
195-268 is the model) whose retired value is the channel's completion value. The Present's private record carries
`encode(ce_gate, value)`; `WddmPending.stream_boundary` (`virtio/gpu/mod.rs` 2036) and `scanout_boundary_ready` (7120)
then work unchanged, and `take_one_ready_wddm` (8179) retires the DMA fence only when the value is reached:
`ce_present::retire` is the rule, `Retire::Discharge` the timeout's counted exception. The gate's value is 32 bits wide; the
channel is rebuilt before its completion value reaches `2^32 - 1025` (about 49 days at 1000 frames per second).

How the KMD learns the value advanced, two ways, both reading the CPU mapping of the completion page:
- **Event (preferred)**: the push's `NON_STALL_INTERRUPT` -> RM event of the KMD client -> host `EventReady` -> the existing
  `nvrm_events` DPC (`virtio/gpu/nvrm_events.rs` `drain_nvrm_events` 560, `deliver_nvrm_ready`) recognizes the KMD's own
  event handle, reads the completion value, `Ring::observe`, advances the gate, and the same DPC pass re-evaluates the WDDM
  FIFO head (as a fired RM gate point does today). Edge-triggered: every pass reads the value, never counts events.
- **Poll (fallback, and the timeout clock)**: the HPD worker and the heartbeat read the value; `Route::poll` with the time of
  the producer's fence (the RM gate point of the same Present fired in the same DPC) runs the timeout of
  `TIMEOUT_AFTER_PRODUCER_MS`. The completion delivery latency of the event path for a GPU release on a KMD-owned client is
  unverified (unknown 2 of 6); M3c measures `CeLatUs` against the poll.

### 11.6 Locking and IRQL

- Order: `scanout_mutex -> venus_mutex -> virtio_lock -> CE` (`adapter/locks.rs`); `CE` is a new leaf spinlock over the ring
  (`Ring`), the gate's value and the per-destination `Route`s. Nothing is allocated, waited on or sent under it; the
  slot write, the entry, `GP_PUT` and the doorbell are plain stores and run under it at DISPATCH.
- Present: PASSIVE, takes `CE` alone (no Venus or virtio lock) for submit; the private-record merge is the existing code.
- DPC: under `virtio_lock` (where `drain_nvrm_events` runs), then `CE` to observe; it signals the worker with
  `KeSetEvent(Wait = FALSE)` as today.
- RM calls (bring-up, dup, map, descriptor, teardown): PASSIVE, no spinlock held, through the RM client's `Io` with its
  bounded message timeouts; a teardown that must wait for the GPU polls the value with `sleep_ms` (as `blt_async::drain`).
- Eviction: `BuildPagingBuffer` already serializes the guest-blob retire (`system_backings.serialize`); the copy-engine retire
  runs first inside the same serialization.

### 11.7 Unverified items

1. The source mapping kind (10.3): a block-linear source mapped by the KMD with kind 0x06 reads the right pixels. M1b
   (10.4) cannot settle this, because its round trip passes with any kind; only M3c can.
2. The block-linear words on 0xcab5 against the 610.57.04 headers (its `clcab5.h` omits them; Mesa's has them) and
   `KIND_BPP` (Mesa only). An M1b PASS (10.4) settles the methods; whether the field values match NVK's layout is left to
   M3c. The tool is built but has not run yet.
3. `SEM_EXECUTE.RELEASE_TIMESTAMP` and `NON_STALL_INTERRUPT` on 0xca6f (not in `clca6f.h`; the tool's PASS used
   `NON_STALL_INTERRUPT`).
4. Which notifier index a CE channel's non-stall interrupt raises for an `NV01_EVENT_OS_EVENT` (option B); the tool only
   measured the semaphore-surface fence (option A).
5. The KMD client's device without `OPTIONAL_MULTIPLE_VASPACES` taking a CE channel (unknown 3 of 6).
6. A dup of another guest client's image memory from the KMD client, and its GPU mapping with the image's kind (the tool
   dup'd a pitch source of its own process).
7. Compressed sources (GB20x compressible kinds): refused by the route (`source_plan`) until a tool proves them.
8. The doorbell store from kernel mode through window region 1 (the tool stored from user mode).
9. The copy engine's REMAP unit (`SET_REMAP_CONST_A/B`, `SET_REMAP_COMPONENTS`, `LAUNCH_DMA.REMAP_ENABLE`;
   `ce_present::Remap`, section 12). The fields are identical in Mesa's `clcab5.h`, Mesa's `clc7b5.h` and the 610.57.04
   `clc7b5.h`; the 610.57.04 `clcab5.h` omits them, so 0xcab5 rests on Mesa's header alone. Ada (0xc7b5) is UNVERIFIED on
   hardware (no Ada GPU has run the tool), not because its header differs. Also unverified on both: REMAP together with a
   block-linear source (the open question of 12.3), and which byte of `SET_REMAP_CONST_A` a 1-byte component takes (the
   route sends 0xffffffff, so every byte is 0xff either way).

### 11.8 Work list

**M3b, the channel subsystem (about 1700 lines):**

| file | what | lines |
|---|---|---|
| `kmd_logic/src/ce_channel.rs` (new) | the bring-up/teardown step machine (VA space, engine pick from `GET_ENGINES_V2`/`CE_GET_CAPS_V2` replies, TSG, subcontext, channel, BIND, CE object, token, schedule, USERD, ring, doorbell, completion), the parameter encoders with byte tests (`NV_CHANNEL_ALLOC_PARAMS` 376 B, `NVB0B5_ALLOCATION_PARAMETERS`, `NV_HOPPER_USERMODE_A_PARAMS`), the engine-type mapping `COPY(n)` | 500 |
| `kmd_render/src/virtio/rm_client/channel.rs` (new) | the I/O of those steps through `Io::rm_alloc`/control, the window maps (reusing `sysmem.rs` `alloc_sys` and the `rm_client.rs` map steps), the first push, teardown, device loss | 600 |
| `kmd_render/src/virtio/rm_client/ce_map.rs` (new) | dup + `NV50_MEMORY_VIRTUAL` + `MAP_MEMORY_DMA` with a kind, unmap/free; the OS descriptor from page runs; the source/timeline cache | 400 |
| `kmd_render/src/virtio/rm_client.rs` | the level, the service hook, `forget`/`retire_begin` reaching the channel | 80 |
| `kmd_render/src/diag.rs` | `KnobName::new(b"RmCopyEngine")` | 10 |
| `kmd_render/src/adapter/locks.rs` | the `CE` leaf lock | 40 |
| hardware | channel up/down 100 times, error notifier 0, `crm_object_count` equivalent 0, the doorbell from kernel mode | |

**M3c, Present integration (about 1300 lines, plus the NVK/UMD side of 10.2):**

| file | what | lines |
|---|---|---|
| `kmd_render/src/ddi/submit_command.rs` | parse and validate the record beside the fence tail (`HERF` 72, `HEPR` 96), the client check, the stash | 90 |
| `kmd_render/src/ddi/onscanout.rs` or the context | the stash slot and its pairing/orphan rule | 40 |
| `kmd_render/src/ddi/ce_present.rs` (new) | knob, counters (`ce_present::COUNTERS`, exact-list test replaces `the_names_are_free_in_kmd_render`), `try_ce`, the destination record hooks | 550 |
| `kmd_render/src/ddi/display.rs` | the call before `try_async`, the `after` bound on the fallback | 50 |
| `kmd_render/src/virtio/gpu/rm_gates.rs` (or `ce_gate.rs`) | the copy-engine gate: encode, observe, purge on transport loss | 150 |
| `kmd_render/src/virtio/gpu/nvrm_events.rs` | the KMD's own event -> observe -> re-evaluate | 50 |
| `kmd_render/src/adapter/backing.rs`, `ddi/guest_blob.rs` | the descriptor state beside the guest blob, the retire order, `CeLeak` | 200 |
| `protocol/src/rm_fence.rs`, `helios_nvrm_escape.h` | the `QUERY_CAPS` bit that tells the UMD to send the record | 20 |
| `kmd_logic` | tests for the stash and the gate arithmetic | 150 |
| NVK (`patches-windows`, new), DXVK bridge, `umd/src/forward/present.rs` | `queue_rm_fence_v3` and the 168-byte `HERF` (10.2) | NVK session |
| hardware | Heaven windowed: `CeHit` = Presents, `CeFallback` 0, pattern equal to the Venus copy, then the GuestBlob sign-off procedure of `kmd-handoff-2026-10.md` 4 | |

Naming: the knob is `RmCopyEngine` (section 6 and 9 called it `BltRmCe`; the M3 knob list uses the longer, unambiguous
name, still within 14 characters).

### 11.9 M3b as built: the channel in the KMD's own RM client

Built, host-tested where pure, type-checked against the stub WDK (`tools/kmd-dev/stubcheck.sh`), never compiled against the
real WDK and never run:

| file | what |
|---|---|
| `kmd_logic/src/rm_ce_channel.rs` | the parameter blocks (device, VA space, TSG, subcontext, channel, copy object, usermode, the controls, `NV50_MEMORY_VIRTUAL`, `MAP_MEMORY_DMA` / `UNMAP_MEMORY_DMA`), pinned by tests to the bytes the tool's own fill code writes (the tool's structs and fill code compiled on the host, every nonzero byte printed: the module docs say how); the memory layout; the engine pick; the generation from the class list; the bring-up stage machine (`Stage`, `BringUp`) and its reverse-order undo (`next_undo`); the service (`Svc`: cold, bringing up, ready, broken, tearing down, cool-down, disabled; `MAX_STRIKES` 3); the deadlines; the self-test rules (`selftest`); `COUNTERS` |
| `kmd_render/src/virtio/rm_client/ce_channel.rs` | the I/O: `ensure_up` (lazy bring-up), `submit`, `poll`, `teardown`, `retire_for_stop`, `drop_views`, `forget`, the knob and the counters (the plan's `channel.rs`) |
| `kmd_render/src/virtio/rm_client/ce_selftest.rs` | the self-test of `RmCopyEngine` = 2 (11.10) |
| hooks | `ddi/hpd.rs` (`ce_channel::service` after `rm_client::service`), `ddi/lifecycle.rs` (`reset_for_start` in `start_generation_mirrors`; `retire_for_stop` in StopDevice after the GuestBlob retire, before the Venus teardown and the transport reset, on the stop budget, and in StartDevice before `retire_transport`), `virtio/rm_client.rs` (the modules; `retire_begin` calls `drop_views`, `forget` calls `forget`), `diag.rs` (`KnobName::new(b"RmCopyEngine")`) |

The plan's `ce_map.rs` (source and timeline dup cache, destination descriptors) and the `CE` lock of `adapter/locks.rs` are
M3c's: the channel's state is one leaf spinlock of its own (`STATE` in `ce_channel.rs`).

**The knob.** `RmCopyEngine` (service-key REG_DWORD), read at every StartDevice and mirrored as `CeKnob` (the value in force):

| value | does |
|---|---|
| 0 (default), and any value not listed | nothing |
| 1 | the Present route (M3c-2, section 15); nothing in M3b |
| 2 | the self-test (11.10), once per transport generation, from the HPD worker |
| 3 | the shadow mode of M3c-1 (section 14): sampled Presents copied again by the channel and compared with the production copy |

What runs at 0: StartDevice reads the knob and writes `CeKnob` = 0 (the read and mirror every per-generation knob does); the HPD
worker pays one relaxed load per pass (`ce_channel::service`); StopDevice and StartDevice pay one relaxed load
(`retire_for_stop`), `retire_begin` one (`drop_views`), `forget` a reset of plain data under the leaf lock. No RM message, no
allocation, no other registry value, no bounded section.

**The bring-up** (`CeChStage` names the stage started last, `CeChFail` = `stage << 24 | kind << 16 | code` of a failure):

| stage | what | messages |
|---|---|---|
| 1 `Client` | the ring client's bring-up machine (`rm_client::Client`, as `sysmem.rs` drives it) with ONE change: the device is allocated with the tool's parameters (`hClientShare` = the client, 64 KiB big pages, `OPTIONAL_MULTIPLE_VASPACES`) | 11 |
| 2 `ClassList` | `GPU_GET_CLASSLIST_V2` on the device: the Blackwell channel, copy and usermode classes if listed, else Ada's | 1 |
| 3 `VaSpace` | `FERMI_VASPACE_A`, index `GPU_DEVICE` | 1 |
| 4 `Engines` | `GET_ENGINES_V2`, `CE_GET_CAPS_V2` per `COPYn` (a refused query is skipped, as the tool does), the pick | 1 + n |
| 5, 6 `Usermode`, `UsermodeMap` | `*_USERMODE_A` under the subdevice (`{bBar1Mapping = 1}` from 0xc661 on), its CPU view | 1 + 4 |
| 7, 8 `Ctl`, `CtlMap` | 8 KiB RM system memory (error notifier at 0, USERD at 4096), its CPU view, zeroed | 1 + 4 |
| 9, 10, 11 `Ring`, `RingMap`, `RingGpuMap` | 128 KiB RM system memory (GPFIFO at 0, the completion value at 4096, the self-test's producer value at 4160, 128 push slots of 512 B from 8192), its CPU view, its GPU mapping at 0x20_0000_0000 | 1 + 4 + 2 |
| 12 to 19 | TSG `{COPY(n)}`, subcontext (SYNC), channel (128 entries, `hObjectError` and USERD = the control memory), `BIND`, copy object `{VERSION_1, COPY(n)}`, `SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX`, `GET_WORK_SUBMIT_TOKEN`, `GPFIFO_SCHEDULE {enable}` | 8 |
| 20 `FirstPush` | `SET_OBJECT` + a WFI release of value 1 (`ce_present`'s words), kicked, polled for 250 ms; the error notifier must be 0 | 0 |

About 40 messages, one 6 s deadline (`BRING_UP_BUDGET_MS`, the sysmem creation budget) for all of them, inside an
`escape_wait` bounded section so every wait primitive under them obeys it; a stage is not started once StopDevice asked the
worker to go. A failed stage gives back everything the earlier ones made, in the tool's teardown order (`next_undo`: schedule
off, the TSG with its children, the ring's GPU mapping, the ring's CPU view, the ring, the control view and memory, the
doorbell view and object, the VA space, the client's files), on its own 3 s allowance; an undo step that fails is counted
(`CeChSoft`) and never retried (what RM may still hold goes with the control file's close, or the transport sweep). The
service then cools down 2 s; three failures in a row disable it for the transport generation. A teardown (after the
self-test, at StopDevice, at a start without a stop) first takes the channel out of the state (no submitter can reach a view
it unmaps), releases every acquire it could wait on (the producer value far ahead), gives what was submitted 250 ms to land,
then runs the same undo.

**Submission and completion.** `submit` (under the leaf lock: plain stores, no RM call) builds `ce_present::present_push`
into the next slot, writes the GPFIFO entry, a full barrier (`mfence`: the write-combined stores drain), `GP_PUT`, a full
barrier, the token to the doorbell, and returns the completion value the push releases. `poll` reads the completion value and
the error notifier through the kernel views and advances `ce_present::Ring`; a set notifier breaks the channel (`CeChanFail`,
`CeNotify`): nothing is submitted until it is torn down. The first cut polls (option C of 4.3); the event path is M3c's.

**How it differs from the tool** (`crm_ce_copy_smoke`):

| | tool (PASSed on GB202) | KMD (M3b) |
|---|---|---|
| client and device | a user-mode client; device `{hClientShare, 64 KiB big pages, OPTIONAL_MULTIPLE_VASPACES}` | a client of its own under the KMD's owner (not the ring client's), the same device parameters: item 5 of 11.7 does not arise |
| VA space | `FERMI_VASPACE_A` index `GPU_DEVICE` | the same |
| generation | `--gen` | `GET_CLASSLIST_V2` (nvk-rm 0003's way) |
| engine | the first async CE | the first async CE with `SYSMEM_WRITE` and not `SHARED`, then one with `SYSMEM_WRITE`, then the tool's rule (7.2) |
| GPFIFO and push slots | an OS descriptor over process pages | RM system memory, cached as the tool's (`RmCeCache` 1: write-combined), CPU view through the RM window armed on a fresh CONTROL file (`MmMapIoSpace` cached, or write-combined with the knob), GPU-mapped snooped with 4 KiB pages |
| completion value | its own 4 KiB of RM system memory | a page of the ring allocation |
| error notifier and USERD | 8 KiB RM system memory, cached | the same (the channel's private memory has no dxgkrnl view, so the level-5 alias concern of `kmd-rm-client.md` 15.5 does not apply) |
| doorbell | the usermode object, `crm_map_memory` through the subdevice, a user-mode store | the same object and the same messages (`RM_MAP_MEMORY` on the subdevice armed on a fresh GPU file, the host's `Mmap`), then `MmMapIoSpace` UNCACHED of that range of the RM window, and the store from kernel mode: item 8 of 11.7, settled by a self-test PASS |
| GPU VAs | packed from 0x20_0000_0000, 2 MiB apart | fixed 64 MiB windows from 0x20_0000_0000 (ring, self-test source, destination), RM's choice if the fixed range is refused, below 2^40 checked |
| GPU timestamps, `--fence` | yes | none: the self-test's times are CPU times (`KeQueryInterruptTimePrecise`), spinning up to 20 ms then 1 ms sleeps |
| teardown check | `crm_object_count` 0 | every undo's RM status (`CeChSoft` 0) and `NvOpen - NvClose` back to its value before the test |

**RM calls the KMD client does not make (yet), and why:**
- `NV01_MEMORY_SYSTEM_OS_DESCRIPTOR` over KMD pages: the page-run registration (`nvrm::pin_pages`, `forward_pinned`) exists only
  for a user process's escape (it locks that process's pages and splices the runs into its own forwarded `RM_ALLOC`); a
  KMD-owned registration is M3c's work (the destination, 11.3). M3b uses RM system memory everywhere instead.
- `NV_ESC_RM_DUP_OBJECT` of another client's memory, `NV_SEMAPHORE_SURFACE`, `NV01_EVENT_OS_EVENT`: not needed by the
  self-test (the producer is a value the KMD writes; completion is polled). M3c.

### 11.10 The self-test (`RmCopyEngine` = 2): what it does and the hardware procedure

Once per transport generation, from the HPD worker at PASSIVE (the first pass with the RM transport up and a VidPn primary
bound), never inside a DDI: bring the channel up; allocate a source and a destination (5763072 B each) of RM system memory
in the channel's client (cached by default, CPU views, GPU mappings); fill the source with a salted position-dependent pattern;
copy 1600x900x4 pitch-linear twice with `ce_present::present_push`:
1. **ready**: the producer value is set before the kick. `CeSelfUs` = kick to completion seen.
2. **wait**: the push acquires a value the producer does not have; the worker sleeps 2 ms (a timer tick), checks the completion
   has NOT landed (else `NotHeld`), sets the producer, and `CeSelfWaitUs` = producer set to completion seen. This copy reads
   the source one word further on, so its destination differs from the first copy's at every word.

Each destination is compared word for word. Then the buffers are freed (after the GPU is idle) and the channel is torn down
(M3b keeps no channel; M3c will). The whole is bounded: 6 s for the bring-up, 4 s for the copies, 250 ms per copy, 3 s for
each undo. A failure is counted, never fatal, and touches nothing of the Present path. The worker is busy for the length of
the test, expected well under a second (most of it the CPU fill and the two compares at write-combined speed).

**Procedure** (main session; the knob is read at StartDevice, so a restart is needed after every change):

```
reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v RmCopyEngine /t REG_DWORD /d 2 /f
pnputil /restart-device "<the Helios display adapter's instance id>"
:: wait for the desktop, then
reg query HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render
```

Repeat the restart five to ten times (one self-test per generation; `CeChSoft` must stay 0 and `NvOpen - NvClose` must come
back to its value without the knob), then set the knob back to 0 and restart.

**Expected values on a PASS** (GB202):

| value | expected | if not |
|---|---|---|
| `CeKnob` | 2 | the knob was not read: no restart since `reg add` |
| `CeSelfTest` | **1** | `0xE0 + stage`: 0xE1 bring-up (read `CeChStage` / `CeChFail`), 0xE2 source, 0xE3 destination, 0xE4 push, 0xE5 ring full, 0xE6 ready copy not seen in 250 ms, 0xE7 ready copy wrong, 0xE8 the acquire did not hold, 0xE9 wait copy not seen, 0xEA wait copy wrong, 0xEB error notifier set, 0xEC out of time |
| `CeSelfWhy` | 0 | RM's `NV_STATUS` of the failing call, or `0x8000_0000 \| kind << 16 \| code` |
| `CeSelfUs` | about the tool's ready doorbell-to-done (below 400 us: the copy is about 200 to 350 us) | |
| `CeSelfWaitUs` | below 50 us plus the copy | |
| `CeSelfPages` | 1407 | |
| `CeSelfMs` | tens of ms cached; hundreds with `RmCeCache` 1 (the fill and the compares at write-combined speed) | |
| `CeChTry` / `CeChUp` / `CeChDown` | 1 / 1 / 1 per generation | |
| `CeChStage` | 20 (the last stage started) | the stage that hung or failed |
| `CeChFail` | 0 | example: 0x0804001F = stage 8 (`CtlMap`), kind 4 (RM), code 0x1F `NV_ERR_INVALID_ARGUMENT` (11.11). `stage << 24 \| kind << 16 \| code`; kinds as `RmFail` (`kmd-rm-client.md` 3); Transport codes 0xE1 budget spent, 0xE2 StopDevice, 0xE3 the first push never landed (the doorbell from kernel mode is the first suspect); Layout 0x60 no known class set, 0x61 no async copy engine, 0x68 a GPU VA at or above 2^40; Rm with `CeNotify` set: the error notifier |
| `CeChSoft` | **0** | undo steps RM or the host did not confirm |
| `CeRmErr` / `CeChanFail` / `CeNotify` | 0 / 0 / 0 | a refused caps query adds 1 to `CeRmErr` and is harmless |
| `CeChan` / `CeChState` | 0 / 0 (torn down, cold, no strikes) | `CeChState` = `phase << 28 \| strikes << 24 \| made bits` |
| `CeGen` | 1 (GB20x; 2 on Ada) | |
| `CeEngine` / `CeCaps` | a `COPYn` type (0x09..0x12, 0x34..0x3d) / its caps, `SYSMEM_WRITE` (0x08) set, `GRCE` (0x01) clear | |
| `CeToken` / `CeRunlist` | the token / its runlist (bits 22:16): not GR's (the tool prints both) | |
| `CeSubmit` | 3 (the first push and the two copies) | |
| `CeCache` | 0 (cached; 1 with `RmCeCache` = 1) | |
| `CeRmCall` / `CeRmStat` / `CeMapNode` | 0 / 0 / 0 | the last failing call (11.11) |
| `CeChMs` | tens of ms | |

A PASS settles items 3 (no `NON_STALL_INTERRUPT` is used), 5 (does not arise) and 8 (the kernel-mode doorbell) of 11.7 for
the KMD client, and that an RM system-memory ring, USERD and notifier (of the kind `CeCache` names) work for a CE channel. It says nothing
about block-linear sources, dups of NVK's memory or the destination descriptors (M3c).

### 11.11 The 348.1 failure: system memory mapped on the wrong kind of file (fixed)

The first hardware run (348.1, 1920x1080@240, `RmCopyEngine` = 2) stopped at stage 8: `CeChStage` = 8 (`CtlMap`),
`CeChFail` = 0x0804001F (stage 8, kind 4 = RM status, code 0x1F = `NV_ERR_INVALID_ARGUMENT`), `CeSelfTest` = 0xE1,
`CeSelfWhy` = 0x1F, `CeRmErr` = 1, `CeChState` = 0x51000000 (cool-down, one strike). Stages 1 to 7 passed, among them the
doorbell's CPU view (stage 6).

Cause, in the KMD: `cpu_map` armed EVERY CPU view on a fresh GPU file (the minor, tied with `REGISTER_FD`), the way
`rm_client.rs`'s level 2 maps VIDEO memory. RM maps system memory only on a control file: librmclient's `map_node_hint`
(`guest/rmclient/src/rmclient.c`) sends `NV01_MEMORY_SYSTEM` and OS descriptors to the control node, and
`win_map_memory` (`transport_windows.c`) opens a fresh control file (no `REGISTER_FD`) for them; a map on the wrong kind
is answered `NV_ERR_INVALID_ARGUMENT` ("wrong channel kind, RM already undid its side"), after which librmclient retries
once on the other kind. The doorbell (BAR memory) is mapped on a GPU file, which is why stage 6 passed. The host passes
`RM_MAP_MEMORY` through as it is (`host/backend/device/src/nvidia/rm_fd.rs` `dispatch_map_memory`: the embedded file
handle translated, the status returned unchanged), and serves a write-combined mapping of system memory for a user client
(the DWM/NVK log): the cache attribute was not the cause. The NVOS33 block matched librmclient's (`rc::nvos33_with_fd`:
hClient 0, hDevice 4 = the device, hMemory 8, offset 16, length 24, flags 44 = 0, fd 48).

Fix: `rm_ce_channel::MapNode` picks the kind by class (system memory and OS descriptors: a fresh control file, no
`REGISTER_FD`; anything else: a GPU file with `REGISTER_FD`), and `cpu_map` retries once on the other kind after RM's
`INVALID_ARGUMENT`, as librmclient does. The kernel view is placed in the region of the map file's device type (the RM
window for both). Every stage that maps system memory had the same fault: stage 8 (`CtlMap`), stage 10 (`RingMap`) and the
self-test's source and destination views (stage 0xE2 / 0xE3 of `CeSelfTest`); allocations (stages 7 and 9, the self-test's
buffers), GPU mappings (stage 11) and the channel objects (12 to 19) never take a map file and were not affected.

Also changed with it:
- **`RmCeCache`** (service-key REG_DWORD, read at StartDevice when `RmCopyEngine` is nonzero, mirrored as `CeCache`): 0
  (default) the channel's own RM system memory is CACHED, as the tool allocates it (`NVOS32_ATTR_COHERENCY_CACHED`), and its
  kernel views are `MmCached`; 1 write-combined memory and `MmWriteCombined` views (M3b's first choice), for an A/B. The
  doorbell stays uncached.
- **The failing call is named**: `CeRmCall` = `esc << 24 | what` (`esc` 0x2b ALLOC with the class, 0x2a CONTROL with the
  command's low 24 bits, 0x29 FREE, 0x4e MAP_MEMORY, 0x4f UNMAP_MEMORY, 0x57 / 0x58 MAP / UNMAP_MEMORY_DMA with the object
  handle's low 24 bits; 0xf1 the `Open` of a map file with its device type, 0xf2 the host's `Mmap`, 0xf3 `MmMapIoSpace`),
  `CeRmStat` = RM's status (or `0x8000_0000 | kind << 16 | code`), `CeMapNode` = `node << 28 | map file handle` of a failed CPU
  view (node 1 control file, 2 GPU file). The 348.1 failure would have read `CeRmCall` 0x4e4d3003, `CeRmStat` 0x1F,
  `CeMapNode` 0x2000_00xx.

The next run reads as 11.10 says for a PASS, with `CeCache` = 0 and `CeRmCall` / `CeRmStat` / `CeMapNode` = 0; then once with
`RmCeCache` = 1 for the A/B (`CeSelfUs`, `CeSelfWaitUs`, `CeSelfMs`).

## 12. Format conversion with the CE remap unit (M1c)

### 12.1 The formats

Heaven's windowed source is RGBA: fourcc `AB24` (`DRM_FORMAT_ABGR8888`, bytes R G B A), DXGI format 28 `R8G8B8A8_UNORM`,
block-linear modifier 0x0300000000606014 (10.3). The redirection surface DWM reads, the Blt destination, is BGRA: `AR24`
(`DRM_FORMAT_ARGB8888`, bytes B G R A), DXGI 87 `B8G8R8A8_UNORM`. Every windowed Present therefore exchanges bytes 0 and 2 of
every pixel. A Vulkan transfer command (`vkCmdCopyImage`, `vkCmdCopyImageToBuffer`) copies bytes and cannot do it, so the
Venus route falls back to the graphics engine for every one of these copies. The copy engine can do it inside the copy: its
REMAP unit picks each destination component from a source component or a constant.

### 12.2 The remap table and the decision

The unit (Mesa's `clcab5.h` and `clc7b5.h`, the 610.57.04 `clc7b5.h`; the 610.57.04 `clcab5.h` omits it, 11.7 item 9):
- `SET_REMAP_COMPONENTS` 0x708: `DST_X` 2:0, `DST_Y` 6:4, `DST_Z` 10:8, `DST_W` 14:12, each `SRC_X..SRC_W` (0..3),
  `CONST_A` (4), `CONST_B` (5) or `NO_WRITE` (6); `COMPONENT_SIZE` 17:16, `NUM_SRC_COMPONENTS` 21:20 and
  `NUM_DST_COMPONENTS` 25:24, each `n - 1`.
- `SET_REMAP_CONST_A/B` 0x700/0x704, sent only when a component selects one.
- `LAUNCH_DMA.REMAP_ENABLE` (bit 10).

With 1-byte components, 4 in and 4 out, component X is byte 0 of the pixel and W byte 3. When the remap is on, the X
quantities of the copy are counted in elements (4 bytes here) instead of bytes: `LINE_LENGTH_IN`, `SET_SRC/DST_WIDTH`,
`SRC/DST_ORIGIN_X`. The pitches stay bytes and Y stays rows. NVK's `nouveau_copy_rect` does the same (`src_bw = 1` with a
remap), and both `ce_present::copy` and the tool follow it.

| source -> destination | remap | `SET_REMAP_COMPONENTS` |
|---|---|---|
| `AB24` -> `AR24` or `XR24`, `AR24` -> `AB24` or `XB24` | `SwapRb`: `DST_X = SRC_Z`, `DST_Z = SRC_X`, Y and W identity | 0x03303012 |
| `XB24` -> `XR24`, `XR24` -> `XB24` | `SwapRb` | 0x03303012 |
| `XB24` -> `AR24`, `XR24` -> `AB24` | `SwapRb` with `DST_W = CONST_A` = 0xffffffff (the X byte is undefined) | 0x03304012, `CONST_A/B` first |
| the same byte order, A or X -> X, or A -> A | `None` (`REMAP_ENABLE` off, a byte copy) | |
| `XB24` -> `AB24`, `XR24` -> `AR24` | identity with `DST_W = CONST_A` | 0x03304210, `CONST_A/B` first |
| anything else | refused: `Unsupported::SourceFormat` (1) or `DestinationFormat` (2), `Why::FormatUnsupported` (13) | |

The decision is pure: `ce_present::remap_for(src_fourcc, dst_fourcc) -> Result<Remap, Unsupported>`. The byte orders
come from `rm_blt::order_for_fourcc` and `swizzle`, the KMD's existing CPU-copy rules, so the two routes cannot disagree.
`dst_fourcc_for_dxgi` maps a Blt destination's DXGI format (87/91 -> `AR24`, 88/93 -> `XR24`, 28/29 -> `AB24`). `Remap` is
`None`, `SwapRb` or `Select(Selector)`, a general selector with the two constants. In M3c the Present arm calls `remap_for`
with the record's `source.fourcc` and the destination's format and sets `CopyRect::remap`; a refusal becomes
`Facts::source = Err(Why::FormatUnsupported)`, and the Present takes the Venus copy. The largest Present push grows to 38
dwords (`PRESENT_PUSH_MAX_DWORDS`). `CONST_A` is 0xffffffff because the headers do not say which byte of the 32-bit constant
a 1-byte component takes; with every byte 0xff the answer does not matter.

### 12.3 The open question: REMAP with a block-linear source

Heaven's copy is a block-linear source into a pitch destination with the remap on. The headers do not say whether the remap
unit works together with block-linear source addressing on 0xcab5, and nothing here has run the combination. NVK's
`nvk_cmd_copy.c` is evidence that it does: it enables the remap on every image copy (`nouveau_copy_remap_format`, an identity
selector with one 4-byte component for a 32 bpp format) and uses 1-byte components for the depth/stencil aspect copies
(`nvk_remap_insert_aspect` / `nvk_remap_extract_aspect`), block-linear images included. But NVK runs those on the graphics
engine's CE of its 3D channel, never with an R/B exchange of 1-byte components on an async CE, so the combination stays
open until the tool has run it. The tool makes
it a run line of its own (`crm_ce_copy_smoke --bl-src-only`): the pitch pattern goes into the block-linear image WITHOUT the
remap (the M1b words), then the image comes back into the pitch destination WITH the remap, and the CPU checks that the
destination is the swapped pattern. A refusal (an RC error on the tool's channel) or a hang (no completion within
`--timeout-ms`) therefore belongs to that combination alone. The tool prints the error notifier and whether the middle
release landed (which copy it stopped in), then tears the channel down with the bounded waits it already has.

### 12.4 The fallback designs, if the combination is refused

Not decided in code; the hardware result chooses. The numbers come from the same matrix run (12.5).
- **A. A second pass.** Copy the block-linear source into a pitch-linear scratch in video memory without the remap (the M1b
  `bl_to_pitch` words, into video memory instead of guest RAM), then copy the scratch into the destination with the remap
  (pitch -> pitch, the `pitch_to_pitch_on` combination), in the same push with a host release with WFI between them. Cost:
  one more 5.76 MB video-memory-to-video-memory copy per frame, about `remap_off_pitch_to_bl_us` (the tool's pitch -> BL copy
  also reads and writes video memory), and a scratch of `pitch * height` bytes (5.76 MB) per channel, enough because the
  channel serializes its frames. Expected total: `remap_off_pitch_to_bl_us + remap_on_pitch_to_pitch_us` per frame, against
  one copy today; the extra pass stays in video memory, and the guest-RAM write over PCIe (the ~0.2 ms) is unchanged. The push
  grows by about 24 dwords (to about 62, still one 512-byte slot).
- **B. The remap while writing the block-linear image.** If the remap works when the block-linear side is the destination
  (`bl_dst_remap`), a copy that WRITES the image can swap. The KMD route writes no block-linear image (NVK's 3D engine
  renders it), so B moves the swap to the producer: NVK presents a BGRA image (a swapchain format or its own blit into one),
  or a producer-side CE copy writes the image with the remap. Cost: no extra pass on the KMD's channel, the same ~0.2 ms
  copy; the work is an NVK change, outside the KMD.
- If neither works, the route takes the Venus copy for every RGBA source (`Why::FormatUnsupported`), as today.

### 12.5 What the tool measures, and the pass criteria

`crm_ce_copy_smoke` (`guest/rmclient/tests/crm_ce_copy_smoke.md`, "Format conversion with the remap unit"):
- `--remap swap-rb`: the measured loop of run 1 with the measured copies remapped; `remap_verify` and a
  `remap_gbps path=pitch_to_pitch on=` line to compare with `copy_gbps_p50` of the same run line without the option.
- `--bl-roundtrip --remap swap-rb`: the matrix. Every combination runs all its rounds before the next: `pitch_to_pitch_off`,
  `pitch_to_pitch_on`, `bl_off` (pitch -> BL -> pitch), `bl_dst_remap` (fallback B's write side), `bl_src_remap` (the open
  question, last). It prints the GPU time of every copy as six stage rows (`remap_off_*` and `remap_on_*` for
  `pitch_to_pitch`, `pitch_to_bl`, `bl_to_pitch`), one `remap_gbps path=... off=... on=... on_vs_off=...%` line per path,
  `remap_accepted:` with `yes`, `NO` or `not-run` per combination, `remap_verify`, and the first 8 mismatches per
  combination. The sizes are Heaven's: 1600x900 and the block-linear image of modifier 0x0300000000606014.
- `--bl-src-only`: `bl_src_remap` alone (12.3).

Pass criteria:
- every combination accepted and `remap_verify=ok` (the destination is the pattern with bytes 0 and 2 exchanged, 0 bad
  words);
- the remap costs at most about 10% of the throughput: `on_vs_off` at 90% or more on every path;
- pitch -> pitch into guest RAM near the M1 rate, about 28 GB/s (5.76 MB in about 0.2 ms);
- the error notifier 0 and nothing left tracked (as M1).

A PASS settles the remap's methods and fields on 0xcab5 against Mesa's header (11.7 item 9) and the open question for the
tool's own block-linear image. As with M1b, the round trip goes through one mapping, so the match with NVK's own layout and
kind is still M3c's check (10.3). Ada (0xc7b5) stays unverified until an Ada GPU runs the tool.

## 13. M3c-0 as built: the KMD parses the record and advertises it

The first step of M3c: the KMD reads the `'HEF3'` record (10) behind a present marker's RM fence tail, counts it, keeps
it beside the fence, and advertises that it does. No route uses it yet: nothing reads the kept record, and every
Present is copied as before. Built, host-tested where pure, type-checked against the stub WDK, never compiled against
the real WDK and never run.

The producer side is the NVK/UMD branch `feat/rm-copy-engine-present-nvk` (its doc section "The NVK and UMD side",
numbered 12 there, needs a new number when the branches meet). This branch carries that branch's protocol files
unchanged (`protocol/src/rm_fence_v3.rs`, `protocol/include/helios_rm_fence.h`, `protocol/include/helios_icd_interface.h`,
`rmclient/src/helios_nvrm_escape.h`), so the two merge without conflicts. Those files bring
`HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3`, the 168-byte `HeliosPresentRefreshCmdRmCopy`, the 192-byte
`HeliosPresentRenderCmdRmCopy` and `producer_record`.

### 13.1 Where the marker is accepted and where the record is read

Both carriers arrive in `DxgkDdiRender` (`kmd_render/src/ddi/submit_command.rs`); `DxgkDdiPresent` never sees command
bytes. The KMD already accepted a longer command before this change, so no check was relaxed:
- the only length refusal is `cmd_len > DmaSize` (the runtime grows the DMA buffer and retries). The command is copied
  whole into the DMA buffer;
- `HERF`: decoded when `cmd_len >= 16`, with the first 32 bytes copied into a local. The fence tail is read at 32 when
  `cmd_len >= 48`, and the on-scanout slot at 48..72 (`onscanout::note_render`). That slot is zero in the 168-byte form
  and parses as "no tag". Nothing past 72 was read;
- `HEPR`: decoded when `cmd_len` covers the 48-byte prefix, with the first 80 bytes copied into a local. The fence tail
  is read at 80 when `FLAG_RM_FENCE` is set and `cmd_len >= 96`. Nothing past 96 was read.

An older KMD therefore reads a 168- or 192-byte command as the 48- or 96-byte one, as `rm_fence_v3.rs` states.

The record is read in exactly one place per carrier: the arm where the fence tail goes to `attach_or_take_fence_tail`
(a fence tail and an all-zero stream tail). `ddi/ce_record.rs::note_render` then does the following:
1. It returns at once unless the tail is a FENCE tail (`HeliosRmFenceTail::is_fence`).
2. It copies at most 96 bytes, once, from offset 72 (`HERF`) or 96 (`HEPR`) into a local and runs the protocol's parser
   on them. A short tail goes through `HeliosRmFenceTailV3::parse`. A full one goes through `validate`, with the
   command's remaining length as the bound for `bytes`, so a later, longer revision is accepted and its extra bytes are
   ignored.
3. It requires `matches_fence` against the tail just read (`semaphore.value == rm_fence_value`).
4. It counts the outcome (13.2) and stashes a valid record on the context as `StashedCeRecord { boundary, record }`.
   The slot is `ContextContext::ce_record`: plain data under a leaf spinlock with a lock-free flag, like the on-scanout
   tag. `boundary` is what `rm_gate_attach` returned for the record's own fence, and a fence that was not attached
   stashes nothing. Each fenced Render replaces the stash, so a record lives until the next fenced Render of the
   context. `take_ce_record(boundary)` has no caller yet; it hands the record only to the Present whose stashed marker
   is `StashedMarker::Resolved` with the same boundary.

The record never changes the fence. It is parsed after the attach, and the attach, the marker stash and the scanout
refresh run exactly as before. A refused record is counted and never fails the Render.

**The `h_client` rule** (both `h_client`s must be RM clients the presenting process created) is checked by the route
at the Present that would use the record, not here: `record_client_owned_by_presenter` (15.3). The stash keeps what
the producer sent, and nothing acts on it before that check.

The shadow mode (M3c-1, `RmCopyEngine` = 3) dups what a record names WITHOUT that check and counts every record it uses on trust (`CeRecClient`, 14.4); it is diagnostic only.

What runs when no record is present:
- a Render without a FENCE tail (Venus stream markers, `HE12`, `HEFL`, every other command): nothing new;
- a FENCE tail in the 48- or 96-byte form: after the unchanged attach, one `is_fence` test, `available = 0` (nothing is
  copied), one relaxed `fetch_add` (`CeRecNoCopy`, with a registry write on the first and every 256th) and one relaxed
  load of the stash flag;
- `DxgkDdiPresent`, SubmitCommand, the DMA buffer and the private record: unchanged.

### 13.2 Counters and the capability

The counters are listed in `kmd_logic::ce_record::COUNTERS` and written only by `ddi/ce_record.rs`. An exact-list test
checks that file and checks that no other file spells the names; `rm_ce_channel`'s name scan admits that one file. The
values live in atomics in the Render DDI. The registry is written at PASSIVE only: from the Render DDI on the first
event, on a new refusal reason, and on every 64th refusal or 256th record, and otherwise from `publish_nvrm_counters`
(the throttled block). All of them are zeroed at StartDevice.

| value | meaning |
|---|---|
| `CeRecSeen` | records that passed `validate` and `matches_fence` (kept beside the fence when it was attached) |
| `CeRecBad` | records refused |
| `CeRecWhy` | the last refusal. 1..14 are the protocol's `TailV3Error` in declaration order (`BadMagic`, `Short`, `Version`, `Flags`, `Incomplete`, `Reserved`, `Handle`, `SemaphoreOffset`, `Value`, `Dimensions`, `Format`, `Pitch`, `Modifier`, `Size`); 15 means the semaphore value is not the fence's |
| `CeRecMask` | every refusal seen, bit `code - 1` |
| `CeRecNoCopy` | FENCE tails without a record (the 48- and 96-byte forms) |
| `CeRecLast` | `semaphore.h_client` of the latest kept record (REG_DWORD) |
| `CeRecMod` | `source.modifier` of the latest kept record (REG_QWORD) |

The route's planned `CeTail*` names in `ce_present::COUNTERS` stay unwritten; M3c decides whether it still needs them
beside these.

`QUERY_CAPS.supported_ops` bit 37 (`HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3`) is part of `NVRM_OPS_IMPLEMENTED`
(`ddi/escape.rs`). This build always sets it, whatever `RmCopyEngine` says: the record is an input, and the route
decision stays per frame. A `kmd_logic::ce_record` test pins the bit in the protocol's Rust and C sources and in that
mask.

### 13.3 Hardware procedure (main session)

Builds: this KMD, plus the UMD and the NVK series of `feat/rm-copy-engine-present-nvk`. The NVK series must include
`patches-windows/0053` (`helios_icd_interface` version 6, `queue_rm_fence_v3`), and the D3D11 UMD runs with
`NvkRmCopyRecord` = 1 (the default). The composed RM-fence path must be on, which it is by default (`NvkRmFence`,
`NvkRmFencePresent`). `RmCopyEngine` stays 0: M3c-0 does not need it, and nothing reads the record anyway.

1. Install the KMD and the UMD/NVK builds, restart the device, and wait for the desktop.
2. Run a windowed (composed) DXVK-on-NVK app (Heaven windowed at 1600x900) for about a minute.
3. Read `reg query HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render` twice, about 10 s apart, and read the UMD
   log.

Expected:

| what | expected | if not |
|---|---|---|
| bit 37 | the UMD logs `NVK present: copy-engine record: semaphore 0x<client>/0x<memory>+<offset> value <v>, source ...` once. The UMD sends the record only after it reads bit 37 in its cached `QUERY_CAPS` | `copy-engine record not sent: <why>` gives the UMD's reason. No line at all: bit 37 was not seen, `NvkRmCopyRecord` is 0, or NVK lacks `queue_rm_fence_v3` |
| `CeRecSeen` | grows between the two reads, about one per composed frame, in step with `RmGAtt` | 0 while `CeRecNoCopy` grows: the UMD sends the 48- or 96-byte forms |
| `CeRecBad` / `CeRecWhy` / `CeRecMask` | 0 / 0 / 0 | the producer and the KMD disagree on a rule. The UMD's `producer_record` applies the same checks, so any value is a bug; `CeRecWhy` names the rule |
| `CeRecNoCopy` | 0, or small (frames whose image NVK could not describe, `VK_INCOMPLETE`) | |
| `CeRecLast` | the NVK device's root client, equal to the `semaphore 0x<client>` of the UMD's log line | |
| `CeRecMod` | 0x0300000000606014 for Heaven's block-linear source (10.3), 0 for a LINEAR one | another value: compare it with the UMD log's `modifier` |
| the desktop and the app | frames on screen, everything else as without the record | |

A PASS shows that the record arrives intact and that the KMD reads it. It says nothing about the copy itself (M3c).

## 14. M3c-1 shadow mode as built (`RmCopyEngine` = 3)

The second step of M3c: before the copy-engine route owns any Present, the KMD runs the copy-engine copy of a REAL Present's
source into a scratch buffer and compares it with what the production (Venus) copy wrote into that Present's destination. That
proves the source layout, the page kind of the mapping (10.3, 11.7 item 1), the remap (12.3, 11.7 item 9) and the acquire
against real NVK images, with the production copy as the reference. Nothing the Present path does changes: the shadow copy
writes only the KMD's own scratch. Built, host-tested where pure, type-checked against the stub WDK, never compiled against the
real WDK and never run.

| file | what |
|---|---|
| `kmd_logic/src/ce_dup.rs` | the dup + map cache's rules: the bounded LRU table (`Cache`: 4 images, 2 timelines), each slot's fixed handles and 64 MiB VA window, the map flags and their fallback (`map_tries`), the mapping lengths, `CeDup*` / `CeMap*` |
| `kmd_logic/src/ce_shadow.rs` | the shadow's rules: the sampling (`capture`), `Skip` and the strikes, the lease coverage and row gathering, the comparison (`Tally`, `Verdict`: equal pixels, R/B-swapped pixels, the bins, the first difference), `CeShadow*` |
| `kmd_logic/src/rm_ce_channel.rs` | `Mode::Shadow` (3), `nvos55` (`NV_ESC_RM_DUP_OBJECT`), `nvos46_kind` (`kindOverride` at 40), the big-page and kind-override flags, `H_SCRATCH*`, `H_DUP_BASE`, `VA_SCRATCH`, `VA_DUP_BASE` |
| `kmd_render/src/virtio/rm_client/ce_dup.rs` | `dup_map_record`, `release_all`, `is_cached`: the I/O of the cache (the interface M3c-2 uses too) |
| `kmd_render/src/virtio/rm_client/ce_shadow.rs` | `note_present` (the Present's hook), `service` (the worker), the scratch, the compare, the counters |
| `kmd_render/src/virtio/rm_client/ce_channel.rs` | the worker's dispatch for knob 3, `gpu_map_with` (flags and kind), the teardown order of the new objects, `try_io` / `end_io` |
| `kmd_render/src/adapter/backing.rs`, `sync.rs` | `try_serialize` (a zero-timeout `PassiveMutex::try_lock`), `SystemBackingSnapshot::reader` (a read-only pin of a destination's leases) |
| `kmd_render/src/virtio/gpu/mod.rs` | `present_buffer_settled`: no KMD writer, no mirror, no queued copy owns the buffer |
| `kmd_render/src/ddi/display.rs` | two calls of `note_present`: at the end of the legacy Blt arm (the copy waited for and mirrored) and where a `BltAsync` arm hands its copy off |
| `kmd_render/src/ddi/ce_record.rs`, `diag.rs` | `CeRecClient`; the knob `CeShadowEvery` |

### 14.1 What one shadow does

1. **Sample** (`note_present`, the Present DDI, PASSIVE). A Blt into a KMD standard-buffer destination whose production copy is
   settled (legacy arm) or handed off (`BltAsync` DIRECT or DEFERRED: the copy owns the destination until it is done) counts
   `CeShadowSeen`. One in `CeShadowEvery` (default 64; 0 is 64; at most 65536) becomes the sample, when no other sample is
   pending or in flight: the Present's record, taken from its context's stash for the Present's own boundary
   (`take_ce_record`), and its destination (resource id, extent, pitch, DXGI format). A later Present to the SAME destination
   before the worker took the sample replaces its record (a newer frame), so a settled destination holds the sample's frame
   or a newer one. A sampled Present without a record is `Skip::NoRecord`. The worker is woken (`signal_hpd`).
2. **Settle and pin** (`service`, the HPD worker, PASSIVE). Each pass: a sample older than `EXPIRE_MS` (500 ms) is dropped
   (`Expired`); otherwise, holding the channel's `IO_BUSY`, the worker TRIES the content transaction and, under it, asks the
   transport whether the destination is live and settled (`present_buffer_settled`). Busy or not settled: retried next pass.
   Settled: a destination without a full system backing, or whose system copy is marked stale (`BltNoMirror`), is
   `NoDestination`; otherwise its leases are pinned (`reader`) and the transaction ends.
3. **Dup, map, scratch** (no lock held, a 2 s bounded section). The channel comes up if it is not (11.9, its own deadline);
   `ce_dup::dup_map_record` (14.2); a scratch of `round_up_page(pitch * height)` bytes in the channel's client (RM system memory
   of `RmCeCache`'s attribute, a CPU view, a GPU mapping at `VA_SCRATCH`), kept for the next sample and grown when needed.
4. **Copy.** The scratch is filled with `POISON` (0x5a5aa5a5), then one push: host `SEM_EXECUTE` acquire of the record's
   `semaphore.value` at the dup's VA + `semaphore.offset`; the copy of `source_plan`'s layout (block-linear with the modifier's
   block height for Heaven) from the image's VA + `source.offset` into the scratch at the DESTINATION's pitch, `line_bytes =
   width * 4`, `height` lines, `remap_for(source.fourcc, dst_fourcc_for_dxgi(destination format))` (Heaven: `SwapRb`); a WFI
   release of the completion value. Polled for `COPY_DEADLINE_MS` (100 ms): 2 ms of spinning, then 1 ms sleeps. The producer's
   value was reached before the production copy ran, so the acquire holds for no time.
5. **Compare.** Row by row: the destination row gathered from its leases (a row may cross leases), the scratch row read through
   its view, each pixel compared under the destination format's mask (an X format's fourth byte is undefined), and also after
   exchanging bytes 0 and 2 of the scratch pixel. Then the transport is asked again whether the destination is still settled,
   and whether a Present to it arrived meanwhile (`DST_GEN`): either is a race (`CeShadowRace`).

The extent must match (`source.width/height` = the destination's), the pair must be one `remap_for` converts, and
`source_plan` must accept the source; otherwise `Unsupported`.

### 14.2 The RM calls of the dup and the map

All from the KMD's own channel client (`h.root`), on its control file, PASSIVE, no lock held, each counted in `CeRmCall` /
`CeRmStat` when it fails (11.11), per producer object ONCE (cached):

| call | block | for |
|---|---|---|
| `NV_ESC_RM_DUP_OBJECT` (0x34) | `NVOS55 {hClient = h.root, hParent = the device, hObject = H_DUP_BASE + 2 slot, hClientSrc = record h_client, hObjectSrc = record h_memory, flags 0}` | the semaphore memory, the image |
| `NV_ESC_RM_ALLOC` of `NV50_MEMORY_VIRTUAL` | the slot's fixed 64 MiB window (`VA_DUP_BASE + slot * 64 MiB`), RM's choice if refused | both |
| `NV_ESC_RM_MAP_MEMORY_DMA` (0x57) | semaphore: `PAGE_SIZE_4KB | CACHE_SNOOP_ENABLE`, the page(s) holding `offset + 8`; image: the whole object (at most 64 MiB), `PAGE_SIZE_BIG | PAGE_KIND_OVERRIDE` with `kindOverride` = the modifier's `k` (0x06), and on an RM refusal `PAGE_SIZE_4KB | CACHE_SNOOP_ENABLE | PAGE_KIND_OVERRIDE` with the same kind; a pitch-linear image without the override | both |
| `RM_ALLOC` `NV01_MEMORY_SYSTEM`, `RM_MAP_MEMORY` on a fresh control file + the host's `Mmap` + `MmMapIoSpace`, `NV50_MEMORY_VIRTUAL` + `MAP_MEMORY_DMA` | 11.9's helpers | the scratch |
| a CPU view of the dup'd semaphore memory (as the scratch's) | only after a copy did not complete | `CeShadowSem` |

The dup's class is not part of `NVOS55` (librmclient's `crm_dup_object` takes one only for its own bookkeeping): the KMD never
names it, so it works for system and video memory alike. The cross-client dup goes through `nvrm::forward` as the KMD's owner,
which `NvDupHarden` never judges (`nvrm_harden::mode_for`), and the host passes it through (`vidmem.rs` only notes a confirmed
one). A refusal therefore comes from RM: `CeDupStat` holds its `NV_STATUS` (or `0x8000_0000 | kind << 16 | code` for a
transport or host refusal); `CeRmCall` is `0x34 << 24 | the object's low 24 bits`.

Calls the KMD does NOT make, and why: no `NV_SEMAPHORE_SURFACE` and no `BIND_CHANNEL` (the acquire is a host-method semaphore
read of plain memory, as the tool's); no OS descriptor over the destination's pages (M3c-2's work: the shadow's destination is
its own RM memory); no `UNMAP` of the image before an eviction other than the LRU's.

Teardown: `release_all` gives the slots back youngest first (`UNMAP_MEMORY_DMA`, the free of the virtual allocation, the free
of the dup), the scratch likewise (its CPU view, its GPU mapping, its memory). `ce_channel::undo_all` runs both BEFORE the
channel group when the GPU is idle (the reverse of their making), right AFTER the group's free when a copy may still run (a
stuck acquire: the group's free stops the channel), and once more before the client's files close (a no-op when empty). With
StopDevice's flag up or the budget spent nothing is sent (kernel views are still unmapped) and the client's close takes the
rest. `drop_views` / `forget` unmap the scratch's kernel view and forget the table at a transport loss.

### 14.3 Locking, IRQL, the Present path

- **With `RmCopyEngine` 0, 1 or 2**: `note_present` is one relaxed load; the worker's dispatch never reaches the shadow; the
  record stash is never taken; nothing is allocated, sent or written to the registry. StartDevice reads nothing new unless the
  knob is nonzero (the `CeDup*` zeros are written for any nonzero knob, `CeShadowEvery` is read only at 3).
- **The Present path in shadow mode**: the `SAMPLE` leaf spinlock (twice), for a sampled Present the context's `ce_record` leaf
  spinlock (`take_ce_record`, after `SAMPLE` was released) and a `KeSetEvent`. It never waits for the shadow.
- **The worker**, in this order: the channel's `IO_BUSY` (compare-exchange, never waited for) -> the content transaction
  (`try_serialize`, a zero-timeout wait: never waited for) -> the virtio spinlock inside it (content -> Venus -> virtio is the
  documented order, 24.12.4 of `zero-copy-present.md`); both released before any RM message. Then, without either: the
  channel's `STATE`, `ce_dup`'s `CACHE`, `SCRATCH` (leaf spinlocks over plain data, never held across I/O), and the virtio
  spinlock once more for the race check. No Venus mutex, no scanout lock. Registry writes only from the worker (PASSIVE).
- **Reading the destination without the transaction**: the reader keeps the leases locked and mapped, so the read is
  memory-safe. A later Present's copy into the same pages may overlap it: detected (`CeShadowRace`), not prevented. Holding the
  transaction for the compare (a few ms of 5.76 MB) would make that Present's mirror wait, which the shadow must never do.
  A pinned reader delays the unlock of pages a concurrent eviction replaced by the length of one compare.
- **Bounds**: the RM messages of an attempt share a 2 s deadline in a bounded section (`escape_wait::begin_bounded`); the copy
  100 ms; the sample 500 ms; the channel's first bring-up its 6 s (11.9). StopDevice joins the worker first; the shadow's
  objects go in the channel's teardown on the stop budget.
- **Strikes**: `DupRefused`, `MapFailed`, `NotReached`, `Channel`, `Scratch` are strikes; three disable the mode for the
  transport generation (`CeShadowStrk`), and the channel is torn down then. A `NotReached` or `Channel` skip tears the channel
  down at once (the next sample brings it up again, under the channel's own strikes and cool-down).

### 14.4 The `h_client` rule and its security consequence

The record's two `h_client`s must be RM clients the presenting process created (the `NvDupHarden` rule, 2.3). The KMD keeps RM
clients per NVRM owner, not per process, so `record_client_owned_by_presenter` still answers `Unknown` (13.1). The shadow mode
ACCEPTS `Unknown` and counts each such record once (`CeRecClient`, a `ce_record` counter, so it reads as "records used on
trust"); with any other knob value nothing is counted.

Consequence, while the knob is 3: a process that sends a well-formed record naming another process's client and memory makes
the KMD dup that memory into its own client and copy it into the KMD's scratch. Nothing of it reaches any process (the scratch
is the KMD's, only counters come out), but the dup keeps the other process's memory alive on the host until the cache evicts
it, and the copy reads it. That is acceptable for a diagnostic knob that is off by default and set only on a test machine; it is
NOT acceptable for the route. **M3c-2 (`RmCopyEngine` = 1) must not call `dup_map_record` before the hook answers `Owned` for
both clients** (and must refuse `NotOwned`).

### 14.5 Counters

`CeDup*` / `CeMap*` (`kmd_logic::ce_dup::COUNTERS`, written by `ce_dup.rs`), `CeShadow*` (`kmd_logic::ce_shadow::COUNTERS`,
written by `ce_shadow.rs`), `CeRecClient` (`ce_record`). Exact-list scans check each list against its writer. Atomics; the
registry is written by the worker after each attempt and by the channel's publish.

| value | meaning |
|---|---|
| `CeShadowEach` | the sampling period in force |
| `CeShadowSeen` | Presents seen in shadow mode (the two hook sites) |
| `CeShadowN` | samples the worker took out (attempted) |
| `CeShadowOk` / `CeShadowBad` | compares with every pixel equal / with a difference |
| `CeShadowPct` | percent of equal pixels of the last compare (rounded down) |
| `CeShadowRow` | `row << 16 | x` of the last compare's first differing pixel |
| `CeShadowP100` / `P99` / `P90` / `PLow` | compares by percent of equal pixels: all, >= 99, >= 90, < 90 |
| `CeShadowSwp` / `CeShadowSwPct` | compares whose scratch is the destination with R and B exchanged (>= 99 % equal after the exchange, more than without it) / the percent equal after the exchange in the last compare |
| `CeShadowRace` | compares during which the destination was presented again or stopped being settled |
| `CeShadowSkip` / `CeShadowWhy` / `CeShadowMask` | samples not compared / the last reason / every reason (bit `code - 1`): 1 no record, 2 no destination, 3 busy, 4 dup refused, 5 map failed, 6 not reached (copy not done in 100 ms), 7 unsupported, 8 channel, 9 expired, 10 disabled, 11 scratch |
| `CeShadowUs` / `CeShadowDupUs` / `CeShadowCmpUs` | microseconds of the last attempt: kick to completion seen / dup + map (near 0 when cached) / the compare |
| `CeShadowSem` | the producer's semaphore value (low 32 bits) read through the KMD's dup after a copy did not complete; 0xffffffff when it could not be read |
| `CeShadowStrk` | strikes this generation |
| `CeDupN` / `CeDupOk` / `CeDupFail` / `CeDupStat` | dups sent / confirmed / refused / the last refusal's status |
| `CeMapOk` / `CeMapFail` / `CeMapStat` / `CeMapFlags` | GPU maps of a dup made / refused after the fallback / the last refusal's status / `kind << 24 | flags` of the last image mapping (0x06080200: big pages with kind 0x06; 0x06080110: the system-memory fallback) |
| `CeDupLive` / `CeDupFree` / `CeDupEvict` | live slots (`semaphores << 8 | images`) / slots given back / evictions |
| `CeRecClient` | records the shadow used although the `h_client` rule was not answered |

### 14.6 Hardware procedure (main session)

Builds: this KMD plus the UMD and NVK series of 13.3 (the record must arrive: `CeRecSeen` grows). The knob is read at
StartDevice, so restart after every change.

```
reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v RmCopyEngine /t REG_DWORD /d 3 /f
reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v CeShadowEvery /t REG_DWORD /d 64 /f
pnputil /restart-device "<the Helios display adapter's instance id>"
:: wait for the desktop; run Heaven windowed at 1600x900 for about a minute; then, twice about 10 s apart:
reg query HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render
```

Then `RmCopyEngine` back to 0 and restart. Read: `CeKnob` (3), `CeRecSeen`, `CeRecClient`, every `CeShadow*`, `CeDup*`,
`CeMap*`, the channel's `CeChUp`, `CeChFail`, `CeChSoft`, `CeRmCall`, `CeRmStat`, `CeNotify`, `CeEngine`, and the desktop and the
app (frames on screen, nothing else changes). A minute of Heaven at about 60 to 200 frames per second gives about 50 to 190
samples.

### 14.7 What each outcome means

| reading | meaning | next |
|---|---|---|
| `CeShadowOk` grows, `CeShadowP100` = `CeShadowOk`, `CeShadowBad` 0 or only with `CeShadowRace` | the copy-engine route reproduces the production copy bit for bit: layout, page kind, remap and acquire are right for real NVK images | M3c-2 |
| `CeShadowBad` grows in `P99`/`P90` with `CeShadowRace` near it, `CeShadowRow` varying | the frames differ in a few rows: the app rendered the next frame into the source while the copies ran (single-buffer reuse), or a later Present overlapped the compare. Not a bug of the route | none; lower `CeShadowEvery` for more samples |
| `CeShadowPLow` with `CeShadowPct` near 0 and `CeShadowSwp` 0, `CeShadowRow` 0 (first pixel) | the copy reads the wrong bytes: the block-linear layout (block height, `KIND_BPP`) or the page kind of the mapping is wrong for NVK's image (10.3). A `CeMapFlags` of 0x06080110 says the image was mapped as system memory | compare with `CeRecMod`; try another kind (a code change in `map_tries`) |
| `CeShadowSwp` grows, `CeShadowSwPct` about 100 | the remap ran in the wrong direction (or where none was due) | check `remap_for` against the formats (`CeRecMod`, the destination's `PBdFmt`) |
| scratch still `POISON` (`CeShadowPct` 0, `CeShadowRow` 0, not swapped) with `CeShadowUs` small | the copy completed without writing the scratch | the push words of the destination (`ce_present::copy`) |
| `CeShadowWhy` 4, `CeDupFail` > 0 | the cross-client dup is refused by RM; `CeDupStat` is RM's `NV_STATUS` as it is (look it up in `nvstatuscodes.h`); a value with bit 31 set is a transport or host refusal (`kind << 16 | code`, kinds as `RmFail`) | the dup path (parent, client handle translation) |
| `CeShadowWhy` 5, `CeMapFail` > 0 | the GPU map of a dup failed both ways; `CeMapStat` says why | the map flags or the kind |
| `CeShadowWhy` 6 | the copy did not complete in 100 ms: `CeShadowSem` below the record's value means the acquire waits on the right memory for a value it does not have (a wrong `semaphore.offset` or value); a value that is nonsense means the wrong address; `CeNotify` nonzero means an RC error | the record's semaphore fields, or the channel |
| `CeShadowWhy` 8 | the channel: `CeChFail` / `CeChStage` (11.10) | 11.10, 11.11 |
| `CeShadowWhy` 2 | the destination has no full, valid system backing (BAR-resident, or `BltNoMirror` marked it stale) | expected for some destinations; none compared means the shadow cannot see this workload's destinations |
| `CeShadowWhy` 1 or 9 only | no record on the sampled Presents, or the destination never settled in 500 ms | the UMD's record (13.3), or the worker's wakes |
| `CeShadowStrk` 3 | the mode stopped for this generation; `CeShadowMask` says which failures | the first failure's reading above |

### 14.8 Unverified

1. Everything here on hardware: nothing has run.
2. The cross-client dup of NVK's memory from the KMD's own client (11.7 item 6), and its map with big pages and kind 0x06.
3. That a dup'd system-memory semaphore read by the host semaphore acquire sees NVK's 3D channel's release (coherent, snooped).
4. That `present_buffer_settled` at the two hook sites means "this Present's production copy is in the pages" for every arm:
   the legacy arm (waited and mirrored before the hook), DIRECT (owned by the copy from the enqueue), DEFERRED (queued before
   the hook). A destination the GuestBlob copy writes directly has no mirror; its system pages are the copy's target.
5. `try_lock` by a zero-timeout `KeWaitForSingleObject` on a `KMUTEX` returning `STATUS_SUCCESS` exactly when acquired.
6. The compare's cost through the scratch's CPU view: cached by default; with `RmCeCache` 1 (write-combined) reads are slow
   (expect `CeShadowCmpUs` in the hundreds of ms).
7. A destination whose format is an X format with a source that has alpha: the fourth byte is masked out of the compare.

## 15. M3c-2 as built: the Present route (`RmCopyEngine` = 1)

Built, host-tested where pure (`kmd_logic/src/ce_route.rs`, the client table's process tag), type-checked against the
stub WDK, never compiled against the real WDK and never run. The producer's dup and GPU mapping
(`virtio/rm_client/ce_dup.rs`) is a STUB on this branch that answers "not implemented" (`Layout` 0x7E): until the
M3c-1 branch's real module replaces it, every routed Present falls back at its dispatch (`CeRtWhy` 19) and the route
exercises only its decision, its queueing, its destination descriptors and its fallbacks. With the knob at anything but 1
(0, 2 the self-test, 3 the shadow mode of M3c-1) the Present path is the one it was: every entry point is one relaxed
load. Section 14 is left for M3c-1.

### 15.1 What happens to a windowed Blt

The route rides the `BltAsync` DEFERRED machinery (`zero-copy-present.md` 24.3) instead of building a second one, so the
ordering, ownership, ledger and fence rules of a deferred copy hold for it unchanged:

1. **Present** (`ddi/display.rs`, Blt arm, `entry.async_enter` branch, before `blt_async::try_async`):
   `ce_present_route::try_route` takes the record stashed for THIS Present's fence (`take_ce_record(boundary)`: the
   boundary `rm_gate_attach` returned for the record's own fence, so an older record never pairs with a newer Present),
   decides (15.2), and if routed queues the request exactly as `blt_async::deferred` does: the Venus copy is PREPARED
   (`prepare_present_blt_guest`: it is the fallback), `queue_async_blt` queues the WindowedBlt request under the
   producer's boundary, a JOB keyed by `(token, boundary)` is recorded in the route's table, the token is merged into the
   Present's private record, `present_complete` merges the boundary, and the DDI returns without waiting (`PBCpy` 5).
2. **Worker, before the dispatch** (`ddi/hpd.rs` -> `ce_present_route::service`): the lazy channel bring-up a Present
   asked for (never inside a DDI), the completions, a broken channel's teardown, and the PREPARATION of each queued job:
   the destination's OS descriptor (15.4) and the producer's dup and mapping (`ce_dup::dup_map_record`).
3. **Dispatch** (`service_windowed_blt`, its rules unchanged: admitted by SubmitCommand, the producer's boundary ready,
   the destination writable and taken as `KmdWriter`): `ce_present_route::dispatch` submits the copy-engine push
   (`ce_present::present_push`: ACQUIRE the record's semaphore value at `Producer.sem_va`, copy
   `Producer.src_va + SourcePlan::offset` (pitch or block-linear per the record, remap per `remap_for`) into the
   descriptor's VA, RELEASE the next completion value with WFI) through `ce_channel::submit` (plain stores; the doorbell is
   a plain store). Anything missing (job, preparation, descriptor, channel) or a refused submit: the request's own Venus
   copy runs, as it would have without the route (`CeRtDispFall`).
4. **Completion** (polled: `ce_present_route::settle`, from the worker's pass and a spin of at most 750 us right after a
   dispatch; the worker's wait is 0.5 ms while copies are in flight): `completed >= value` (`ce_present::retire`) ->
   `VirtioGpu::complete_ce_blt` = the request's ring completion with the mirror forced OFF: the destination goes back to
   `ExternalReady`, the source's ledger ticket retires, the Level 5 edge is raised, the token terminalizes, and the
   Present's DMA fence (which waits for that terminal and for the producer's boundary, `note_wddm_submission`) retires.

Why the dispatch waits for the producer's boundary although the push acquires: a copy submitted before the producer
finished would hold the CE channel on an acquire that a killed producer never satisfies, and every destination's copies
would queue behind it. With the dispatch after the boundary the acquire is already satisfied (it stays in the push as the
GPU-side ordering), and the measured deferred wait is the producer itself, not the worker hop (24.11.5). What the route
saves is the Venus ring-1 round trip (0.5 to 1 ms, `BltAsyncLat`) and its host hops, replaced by the CE copy (0.2 to
0.35 ms) and a poll. Dispatching before the boundary is the follow-up once a stuck acquire can be released.

### 15.2 The decision (`ce_route::decide`, in this order; `CeRtWhy` codes)

Consulted only for a Blt the `BltAsync` entry admitted (`BltAsync` 1, `ForeignCopy` 1, a foreign source, a KMD standard
buffer, no snapshot) with `RmCopyEngine` 1. The first refusal wins; the Present continues into `try_async` exactly as
without the route.

| # | condition | refused as |
|---|---|---|
| 1 | the Present carries an RM-fence boundary | 1 `NoBoundary` |
| 2 | a valid record is stashed for that boundary | 2 `NoRecord` |
| 3 | both record clients are the presenter's (15.3) | 3 `Client` (also `CeRtClient`) |
| 4 | the route is not off (three route strikes, or the channel service struck out) | 5 `RouteOff` |
| 5 | the channel is up (else a bring-up is asked of the worker, `CeRtUp`) | 4 `ChannelDown` |
| 6 | `source_plan` accepts the record's source; `remap_for(source fourcc, destination DXGI)`; the record's size is the destination's and a row fits its pitch | 6 `Source`, 7 `Format`, 8 `Extent` |
| 7 | the destination: a standard buffer with leased system backing whose `round_up(pitch * height)` fits a 64 MiB window; its system copy not marked invalid; no other process has it open; no guest blob | 9 `Destination`, 10 `Stale`, 11 `Foreign`, 12 `GuestBlob` |
| 8 | the destination's record: not struck out (3 strikes), not poisoned or leaked, not found uncovered, not being retired | 13 `Struck`, 14 `Poisoned`, 15 `Uncovered`, 23 `Retiring` |
| 9 | the job table (16) and the destination table (8) have room | 16 `Full` |
| 10 | the request is queued and its token merged | 17 `Queue` |

Dispatch-time fallbacks (the queued request's Venus copy runs): 18 `NotReady` (the preparation was not done in time),
19 `Dup`, 20 `Desc`, 21 `Submit`, 22 `ChannelLost`, 23 `Retiring`, and 10 `Stale` (a mark that arrived after the
Present). 19, 20 and a refused submit other than a full ring are strikes against the destination. The rule is tested over
its whole input space against an independent statement of it (`ce_route` tests).

### 15.3 The `h_client` rule

Compared: the presenting context's process token, `ContextHandleRef::creator_process` (the `hKmdProcess` dxgkrnl gave the
D3DKMT device that owns the context, `DXGKARG_CREATEDEVICE`), against the process token recorded with each record client
(`ce_record::client_check`; both must be `Owned`, `ce_record::both_owned`).

Captured: `HELIOS_ESCAPE_NVRM` takes `hKmdProcess` of the escaping device (`DeviceHandleRef::creator_process` of
`hDevice`, as `PRODUCER`, `PRESENT_STREAM` and `ATTACH_RESOURCE` already do) and passes it through `nvrm::forward` to the
`NvDupHarden` bookkeeping: the reply of a forwarded `NV_ESC_RM_ALLOC` of a root class records the client RM minted with
its owner (the escape device), its file and now its process (`ClientTable::commit_in`, read by `process_of`). One process
has one `hKmdProcess` for all its devices: the NVK ICD's escape device and the D3D runtime's device that presents share it
(the present-stream registration already relies on exactly this edge). A client leaves the table with its free, its
file's close, its device's destruction and the transport, so the token of a process that is gone names no client; RM never
has two live clients of one number (a re-minted number evicts the stale entry, process included).

`Unknown` is refused like `NotOwned` (`CeRtClient`): hardening off (`NvDupHarden` 0 records nothing, so the route then
refuses every Present: run it with the default 2 or with 1), a full client table, a client of the KMD's own, a device
without a process token.

The rule is checked TWICE: at the Present, and again on the worker immediately before `ce_dup::dup_map_record`
(`prepare`, with the presenter kept in the job): a client freed since the Present is refused there (dispatch-time 3,
`CeRtClient`, `CeRtDispFall`). The dup cache never reuses a dup across a change of the client table: every commit or
forget of a client (a root allocation's reply, a client free, a file's close, a device's destruction, the transport's
sweep) bumps a generation (`nvrm_harden::client_generation`), and a cache entry made under an older generation is never
a hit (`ce_dup::Cache::set_generation`: it is re-made in its own slot, its objects given back first). A re-minted client
number or a memory handle a process reused at the same size therefore always gets a fresh `RM_DUP_OBJECT`, which RM
resolves against the client as it is now. Residual window: a client freed and its number re-minted for another process
between the worker's second check and RM's execution of the dup (one forwarded message).

### 15.4 The destination: an OS descriptor over the lease pages

- **Rule with `GuestBlob`.** A destination uses one of the two, never both at once: the route refuses a destination with a
  live guest blob (12 `GuestBlob`), and `guest_blob::prepare` makes no guest blob for a destination the route holds
  (`ce_present_route::holds`: a descriptor made, being made, drained or leaked). The fallback of a routed Present is the
  request's prepared Venus copy, i.e. exactly what the destination would have used without the route: the Venus blob with
  the worker's CPU mirror (or `BltNoMirror`'s stale mark). Run the route with `GuestBlob` 0; with both on, the first to
  claim a destination keeps it until its leases change or it is destroyed.
- **Create** (the worker, lazily, for the first routed Present's job): under the content transaction
  (`system_backings.serialize`) re-check the stale mark and the guest blob, snapshot the leases, build the page runs of
  `[0, round_up(pitch * height))` with `guest_blob::build_runs` (whole pages of one lease each, else `Uncovered`), pin
  exactly those leases (`GuestPin`), then, under the channel's `IO_BUSY`, register them: `NV_ESC_RM_ALLOC_MEMORY`
  (`NVOS02` + fd -1, class 0x71, librmclient's flags) on the channel client's GPU file, handle `H_BASE + 0x80 + 2 * slot`,
  with the page-run table appended as the deep block by `nvrm::forward_kmd_registration` (the same table and the same
  splice a user PIN carries: `nvrm::page_run_table` is the PIN's `build_table` factored out, the escape path byte for byte
  as before); then `NV50_MEMORY_VIRTUAL` + `MAP_MEMORY_DMA` snooped with 4 KiB pages at the slot's 64 MiB window from
  `VA_BASE + 32 * VA_WINDOW` (`ce_channel::gpu_map`). RM's refusal frees what was made and unpins; a timeout (the host may
  hold it) keeps the pin (`Leaked`).
- **Retire** (`guest_blob::before_lease_change` and `destination_gone` call the route FIRST, content transaction held):
  (1) the descriptor leaves `Ready` (no new copy: a queued job falls back at its dispatch); (2) wait until the channel's
  completion value reaches the destination's last submitted value, then terminalize what completed; (3) under `IO_BUSY`
  `UNMAP_MEMORY_DMA`, free the virtual allocation, free the descriptor; (4) only after both frees were confirmed, drop
  the pin (PASSIVE, outside the spinlock), so the lease change may unlock the pages. Every wait is an interrupt-time
  DEADLINE (`ce_route::deadline` / `expired` / `left_ms`): the whole hook at most `LEASE_HOOK_MS` (1 s), the drain at
  most 250 ms of it, the wait for `IO_BUSY` at most 250 ms and the free at most what is left; a `sleep_ms(1)` rounds up
  to the timer quantum (about 15.6 ms), so each wait sleeps and then reads the clock again, overshooting by at most one
  quantum (the first version counted sleeps: "250 ms" could be about 3.9 s, on VidMm's paging thread with the content
  transaction held). A drain or a free that does not complete in time: the destination is `Leaked` (pinned until the
  transport generation ends, never routed again, a strike). A destination found uncovered is looked at again after the
  change. The worker never waits for the content transaction (`try_serialize`: busy is `NotReady`, tried again next
  pass).
- **Destroy while a free is in flight.** `destination_gone` never removes a record another thread is freeing
  (`Draining`, the worker's `free_all_descriptors`): it marks it `gone`, and `finish_free` (matched by resource id, the
  slot is not reused while `Draining`) removes it once that free answered, so the pin drops only after the host free was
  confirmed (or the record stays leaked). A leaked record, one with an orphaned pin or with a copy still submitted stays,
  pin and all, until the generation ends.
- **Channel teardown** (it broke, or StopDevice): the descriptors nothing can write are freed first, the producer dups
  next (`ce_dup::release_all`), then the channel (`ce_channel::teardown` / `retire_for_stop`); the destinations' ring values
  are forgotten (`Route::on_channel_gone`, strikes kept). **Generation end** (`rm_client::forget`, after the device reset)
  follows the transport sweep's `PinFate`, exactly as the user pins do (`nvrm::close_all_on_host`; the KMD's client files
  are closed by the same sweep): every close confirmed, every pin goes; any close unconfirmed (or the transport already
  failed), the pin of every destination a descriptor may name (ready, being made, draining, leaked) and every orphaned pin
  is leaked on purpose (`core::mem::forget`): the host keeps its RM files across a device reset and a GPU copy stuck at
  StopDevice (whose teardown gave up) may still write pages it registered. Locked pages leak; DMA into reused RAM would
  corrupt. The fate is sticky until `forget` reads it, so StopDevice's second, idempotent sweep cannot clear it.

**Can a destination with a descriptor be evicted blob-to-system, or paged in over newer pages?** No, by the paging
path's construction (`build_paging_buffer.rs`): system-backing leases are CREATED by a LOCAL_TO_SYSTEM transfer (the blob
copied into VidMm's system pages, `replace_range`) and REMOVED by the SYSTEM_TO_LOCAL page-in (`remove_range`, both
`bar_virtual_transfer_inner` and `bar_transfer`). A descriptor exists only while leases cover the whole destination, so
only while it is system-resident: the blob is then not authoritative and the next transfer is a page-in, which copies the
PAGES (holding the CE frames) into the blob. The route's retire runs before that transfer (`before_lease_change`, both
directions), so the copy the page-in makes is the newest frame, and nothing marks anything. A LOCAL_TO_SYSTEM of a
system-resident range does not occur (VidMm evicts from a segment), and if a partial transfer ever changed part of the
leases, the descriptor is retired first and the next create finds the destination uncovered. The one mark that could
make a page-in skip the pages (`system_copy_invalid`) is set by a skipped LOCAL_TO_SYSTEM (not possible while
system-resident) or by a `BltNoMirror` Venus copy into the blob (which then holds the newest frame itself); the route
refuses a marked destination at the Present and at the dispatch. No code change was needed.

### 15.5 Completion, the fence, and why dxgkrnl cannot hang on it

The Present's DMA fence waits for the request's terminal token (and the producer's boundary), the plumbing a deferred
`BltAsync` copy uses. The terminal comes from exactly one of: the CE completion (`completed >= value`, written by the GPU's
release after the copy's WFI), a dispatch-time fallback (the Venus copy's own ring completion), or a discharge.

- Every submitted copy has a deadline: `COMPLETE_MS` (100 ms, `TIMEOUT_AFTER_PRODUCER_MS`) from its dispatch, which is
  when the producer was already done. Past it the copy is DISCHARGED: its terminal with a failure (the destination is
  handed back, the fence retires), the destination is poisoned and leaked (pinned for the generation, never routed
  again), the channel is marked broken (it may be stuck on that copy) and torn down by the next pass, a route strike.
- A broken channel (error notifier) or a channel that vanished: every submitted copy is discharged at once, its
  destination poisoned and leaked.
- The worker polls every 0.5 ms (rounded to the timer) while copies are in flight; a wedged worker is bounded by the
  existing `WddmHeadMs` rebase, as for any deferred copy.
- The pixels of a discharged frame are NOT guaranteed: the Present completes late (at most 100 ms) with the destination's
  previous content, and a copy that lands after the discharge writes that frame late (a one-frame glitch, never a memory
  fault: the pages stay pinned). The next Present of that destination takes the Venus copy.
- No mirror, no stale mark: the copy engine wrote the very pages the destination's CPU view (DWM) reads; the Venus blob is
  older than the pages from then on, which is the guest blob's invariant (a page-in copies pages to blob; a marked
  destination is refused).

### 15.6 Failure matrix (what the Present does)

| where | what | the Present |
|---|---|---|
| decision | any refusal of 15.2 | continues into `try_async` unchanged (DIRECT, DEFERRED or legacy), counted |
| queue | prepare, queue or token merge refused | the same; nothing queued is left behind (a queued request whose token is refused is cancelled) |
| queue | the job table filled between the decision and the queue | the request is a plain deferred Venus copy (`CeRtWhy` 16) |
| preparation | descriptor: uncovered, stale, guest blob | the job falls back at dispatch; uncovered sticks until a lease change |
| preparation | descriptor: RM refused | falls back, a strike, nothing left pinned |
| preparation | descriptor: timeout | falls back, the pages stay pinned for the generation (`CeRtLeak`) |
| preparation | dup or map refused (the stub always) | falls back, a strike; three strikes: the destination is struck out |
| dispatch | not prepared, channel down, retiring, marked stale | the request's Venus copy |
| dispatch | ring full | the Venus copy, no strike |
| dispatch | another submit refusal | the Venus copy, a strike |
| completion | done | the fence retires on the terminal |
| completion | 100 ms past the dispatch (its OWN deadline: the channel's head) | discharged (`CeRtTimeout`): the fence retires, previous content, its destination struck, poisoned and leaked; the channel is marked broken and torn down by the next pass, ONE route strike |
| completion | queued behind that copy, or in flight when the channel broke or vanished | discharged as a BYSTANDER (`CeRtChFail`): the fence retires, previous content, no strike and no poison; the descriptor's pin is orphaned (pages locked until the generation ends), the descriptor freed, a fresh one made when a channel is up again |
| DoS (accepted, bounded) | a producer names a semaphore value it never releases (its own other memory: the record must match the fence's value, not its address) | its copy holds the single channel until its 100 ms deadline: every destination's copies queued behind it wait (bystanders, uncharged); the attacker's destination is struck and poisoned at once; three such stalls in a generation turn the route off (`CeRtOff`) and every app falls back to the Venus copy. Bounded: at most 3 x 100 ms of delayed Presents per generation, never a hang |
| route | three route strikes | 5 `RouteOff` for the generation (`CeRtOff` 1) |
| paging, destroy | a copy in flight | drained up to 250 ms (clock), the whole hook within 1 s, then freed and unpinned; else leaked |
| destroy | the worker is freeing the descriptor (`Draining`) | marked `gone`; removed by `finish_free` after the free answered |
| worker | the content transaction is held (paging) | `try_serialize` fails: the job stays pending, its dispatch falls back meanwhile |
| StopDevice | live descriptors | freed on the stop budget before the channel; the rest follow the sweep's `PinFate` at the generation end (leaked when any close was unconfirmed) |

### 15.7 Locking and IRQL

- `ce_present_route::STATE`: a leaf spinlock over plain data (jobs, destinations, route strikes). Order: `STATE` -> the
  channel's `STATE` (submit, poll) -> nothing. No allocation, wait, RM message or virtio lock under it; pins dropped only
  outside it, at PASSIVE.
- Present (PASSIVE): the decision takes the virtio lock (the client table, the foreign opens) and `STATE` separately; the
  queue takes scanout -> Venus -> virtio as `blt_async::deferred`.
- Dispatch: under the scanout lifecycle and the Venus mutex (the worker's dispatch), spinlocks only.
- Completion: `STATE`, then (released) the virtio lock (`complete_ce_blt`, a ring completion's code, DISPATCH-legal).
- RM I/O (bring-up, dup, descriptor create and free, teardown): PASSIVE, no spinlock, one thread at a time under the
  channel's `IO_BUSY` (never waited for on the worker; at most 250 ms of interrupt time on the paging path); order
  content transaction -> `IO_BUSY` -> virtio. Each in an `escape_wait` bounded section (`PREP_MS` 2 s, `FREE_MS` 1 s,
  the bring-up's 6 s); the paging hooks' whole bound is `LEASE_HOOK_MS` (1 s, clock).
- The worker takes the content transaction only with `try_serialize` (never waits behind a paging operation).
- The client table's generation (`nvrm_harden`, an atomic) is bumped under no lock of the route; the dup cache reads it
  under its own leaf spinlock.

### 15.8 Knobs

`RmCopyEngine` = 1 (read at StartDevice, `CeKnob`). The route also needs `BltAsync` = 1 and `ForeignCopy` = 1 (it lives in
their branch) and `NvDupHarden` 2 or 1 (the `h_client` rule), and should run with `GuestBlob` 0 and `BltNoMirror` 0 (a
stale mark refuses the route). `RmCeCache` as in 11.11. No new knob.

### 15.9 Counters (`ce_route::COUNTERS`, written only by `ddi/ce_present_route.rs`, mirrored with the NVRM block)

| counter | meaning |
|---|---|
| `CeRtSeen` | Presents that reached the decision |
| `CeRtRouted` | queued for the copy engine (jobs recorded) |
| `CeRtDone` | completed by the copy engine (terminal with success) |
| `CeRtFall` / `CeRtWhy` / `CeRtMask` | fallbacks (decision and dispatch), the last reason, every reason (bit `code - 1`) |
| `CeRtDispFall` | of them, at the dispatch (the queued request's Venus copy ran) |
| `CeRtClient` | refusals by the `h_client` rule |
| `CeRtStrike` / `CeRtStruck` | strikes against destinations / destinations struck out |
| `CeRtChStrike` / `CeRtOff` | route strikes / the route off for the generation |
| `CeRtPoison` / `CeRtLeak` | destinations poisoned / pins kept for the generation (leaked destinations, orphaned bystander pins, pins leaked at `forget` by an unconfirmed sweep) |
| `CeRtTimeout` / `CeRtChFail` | Presents discharged at their OWN deadline (the channel's head: a producer that never signalled, or a hung copy) / discharged as bystanders of a channel failure (never charged to their destination) |
| `CeRtUp` | bring-ups asked for by a Present |
| `CeRtInfl` / `CeRtPeak` | copies in flight now / the most at once |
| `CeRtDecUs` | microseconds in the decision (sum; per Present: `/ CeRtSeen`) |
| `CeRtDupUs` | in the preparation, dup/map and descriptor (sum) |
| `CeRtSubUs` | in the submit (sum; per copy: `/ (CeRtDone + CeRtTimeout)`) |
| `CeRtDoneUs` / `CeRtDoneMax` | dispatch (producer satisfied) to completion seen (sum / max; per copy `/ CeRtDone`) |
| `CeRtPollUs` | in the completion polls and drains (sum) |
| `CeRtDstNew` / `CeRtDstDrop` / `CeRtDstLive` / `CeRtRuns` | descriptors created / freed / live / page runs of the last one |
| `CeRtDirKnob` / `CeRtDir` / `CeRtDirNo` / `CeRtDirWhy` / `CeRtDirUs` / `CeRtLag` | `CeRtDirect` (15.13) |

The CE copies also feed `BltAsyncLat0..7` and `BltAsyncInfl` (they are deferred asynchronous Blts); a discharge counts in
`BltAsyncFail`.

### 15.10 Where the code is

`kmd_logic/src/ce_route.rs` (the decision, the destination record and its retire order, the jobs, the route strikes, the
NVOS02 block, the slots, the deadlines, the names; tests), `kmd_logic/src/ce_record.rs` (`client_check`, `both_owned`),
`kmd_logic/src/nvrm_clients.rs` (`commit_in`, `process_of`), `ce_present::Route::on_channel_gone`;
`kmd_render/src/ddi/ce_present_route.rs` (Present, dispatch, worker, hooks, counters);
`kmd_render/src/virtio/rm_client/ce_route.rs` (bring-up, the dup call, the descriptor, the submit and poll doors, the
teardown); `virtio/rm_client/ce_dup.rs` (STUB). Hooks: `ddi/display.rs` (Present, dispatch), `ddi/hpd.rs` (service,
settle, wait), `ddi/guest_blob.rs` (prepare, lease change, destroy), `ddi/lifecycle.rs`, `ddi/submit_command.rs`
(publish), `virtio/gpu/blt_async.rs` (`complete_ce_blt`, `ce_blt_request`), `virtio/nvrm.rs` (`page_run_table`,
`forward_kmd_registration`, the process argument), `virtio/nvrm_harden.rs`, `virtio/gpu/nvrm_tables.rs`, `ddi/escape.rs`,
`ddi/ce_record.rs` (`record_client_owned_by_presenter`), `virtio/rm_client.rs` (the modules, `forget`),
`virtio/rm_client/ce_channel.rs` (`knob_mode`, `try_io` / `end_io`, `route_view`, `mark_broken`), `adapter/mod.rs`
(`GuestPin`).

### 15.11 Hardware procedure (main session)

Builds: this KMD (with the real `ce_dup.rs` of M3c-1 merged; without it every routed Present falls back at its dispatch,
which is itself a useful first run, below), the UMD and the NVK series of 13.3 (the record must arrive: `CeRecSeen` grows).

1. **Shadow first** (M3c-1's procedure, `RmCopyEngine` = 3): the dup, the mapping and the copy checked against the Venus
   copy without owning the Present. Go on only if that is clean.
2. Set the knobs and restart the device:
   ```
   reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v RmCopyEngine /t REG_DWORD /d 1 /f
   reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v BltAsync /t REG_DWORD /d 1 /f
   reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v ForeignCopy /t REG_DWORD /d 1 /f
   reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v GuestBlob /t REG_DWORD /d 0 /f
   reg add HKLM\SYSTEM\CurrentControlSet\Services\helios_kmd_render /v BltNoMirror /t REG_DWORD /d 0 /f
   pnputil /restart-device "<the Helios display adapter's instance id>"
   ```
   (`NvDupHarden` absent = 2.) Lowest mode first (1920x1080 at 60 Hz).
3. Heaven windowed at 1600x900 (composed, NVK DWM) for about a minute; read the service key twice, 10 s apart.
4. Restart with `RmCopyEngine` 0 and repeat 3: the A/B.

Expected with the real dup (GB202):

| value | expected | if not |
|---|---|---|
| `CeKnob` | 1 | no restart since `reg add` |
| `CeRtSeen` | about the composed Presents (`BltEntryOk`) | 0: the branch is not entered (`BltEntry*`, 24.11) |
| `CeRtRouted` | close to `CeRtSeen` after the first frames | `CeRtWhy` names the refusal |
| `CeRtDone` | close to `CeRtRouted` | `CeRtDispFall` with `CeRtWhy` 18 to 23 |
| `CeRtFall` | the first few Presents (4 `ChannelDown` while the channel comes up, 18 `NotReady` once), then flat | 3: the `h_client` rule (`NvDupHarden`, `NvCliRec`); 12: `GuestBlob` on; 10: `BltNoMirror` on |
| `CeRtStrike` / `CeRtTimeout` / `CeRtLeak` / `CeRtOff` | 0 / 0 / 0 / 0 | the failure matrix's row |
| `CeRtDstNew` / `CeRtDstLive` | the number of window destinations (1 for Heaven) | 0 with `CeRtWhy` 20: RM refused the descriptor |
| `CeRtRuns` | the descriptor's page runs (1 when VidMm's pages are contiguous, at most 1407 for 1600x900) | |
| `CeRtInfl` / `CeRtPeak` | 0 at idle / 1 (one copy per destination at a time) | |
| `CeChan` / `CeChanFail` / `CeNotify` | 1 / 0 / 0 | the channel broke: 11.10's table (`CeRmCall` / `CeRmStat` name a failed map) |
| `BltAsyncFail` / `BltSrcBusy` | 0 / 0 | |
| the window | live, no tearing, colours right (an RGBA source into a BGRA destination: the remap) | R and B swapped: the remap; scrambled: the source mapping's kind (10.3) |

Stage table (divide the sums): the decision `CeRtDecUs / CeRtSeen` (a few us); the preparation `CeRtDupUs / CeRtRouted`
(the first frame of a destination pays the descriptor and the dup, the rest the cached dup); the submit
`CeRtSubUs / CeRtDone` (a few us: no RM call); dispatch to done `CeRtDoneUs / CeRtDone` (expected 0.2 to 0.4 ms plus the
poll's granularity; `CeRtDoneMax` below 2 ms) against the A/B run's `BltAsyncLat0..7` (the Venus ring-1 copy, 0.5 to 1 ms
in 24.11.5) and `BltDeferUs / BltAsyncDefer` (the producer wait, the same in both runs); `CeRtPollUs` is the worker's cost
of polling. PresentMon `msBetweenPresents` and fps against the A/B run.

Without the real dup (this branch alone): `CeRtRouted` grows for the first three frames of a destination, each falls back
at its dispatch with `CeRtWhy` 19, `CeRtStrike` 3 and `CeRtStruck` 1, then every Present is refused with 13 `Struck`;
`CeRtDstNew` 1 and `CeRtRuns` show that RM accepted the OS descriptor over the lease pages (the first hardware check of
a KMD-owned registration). The window stays live (the Venus copy).

### 15.12 Unverified, and known limits

- Everything on hardware. In particular: that RM accepts a KMD-owned OS descriptor registered through `ALLOC_MEMORY` with
  the KMD's own page-run block (the escape path for user pins is proven by `crm_pin_smoke`, the KMD-owned one is not), that
  the CE writes it coherently for DWM's CPU view (3, 11.7), the remap with a block-linear source (12.3), the source
  mapping's kind (10.3), and the real `ce_dup`.
- The poll's latency: the worker's 0.5 ms wait rounds to the timer resolution; the spin after a dispatch covers a copy
  that completes within 750 us. The event route (4.3 option B) is the follow-up.
- A source destroyed while its copy is in flight (about 0.3 ms): the allocation's teardown finds the request dispatched
  and retains the allocation (as for any dispatched request) until the context's teardown.
- A gate that fired with an error status still releases the dispatch: the acquire may then hold the channel until the
  100 ms deadline discharges the copy and tears the channel down.
- One copy per destination at a time (the `KmdWriter` exclusivity of the deferred route), as with the Venus copy.
- The `h_client` rule cannot see a client re-minted for another process between the worker's second check and RM's
  execution of the dup (15.3).
- A timed-out head is blamed as the producer's fault; whether its semaphore never reached the value or the channel hung
  is not told apart (reading the producer's value would need an RM call in the poll). Either way only that destination
  is charged.
- A Venus fallback with `BltNoMirror` 1 between two copy-engine frames marks the destination stale; the next Present is
  refused (10 `Stale`) but a copy-engine frame already queued behind it is refused only at its dispatch.

### 15.13 Direct submission at the Present (`CeRtDirect`)

First hardware run of the route (352.1, Heaven windowed 1600x900): `CeRtSeen` 16404, `CeRtRouted` 16403, `CeRtDone`
16403, one fallback (`CeRtWhy` 4, the first Present), every strike, timeout, leak, poison and off counter 0,
`CeRtDoneUs / CeRtDone` 220 us (max 931), `BltAsyncLat` below 250 us for 94% of the copies (the Venus baseline 250 us
to 4 ms), PresentMon `msInPresentAPI` p50 2.01 -> 1.61 ms. `BltDeferUs / CeRtRouted` was 511 us: the deferred wait,
from the Present to the worker's dispatch once it sees the producer's boundary ready.

`CeRtDirect` = 1 (service-key REG_DWORD, default 0, read at StartDevice only with `RmCopyEngine` 1, mirrored as
`CeRtDirKnob`; anything else is 0) submits a routed copy at its own Present instead. The push already ACQUIREs the
record's value (`ACQ_STRICT_GEQ`), so the GPU, not the worker, waits for the producer (M1: acquire held 1253/1253).

**Where it is submitted: the Present DDI.** `DxgkDdiPresent` runs at PASSIVE and holds no lock where the route has
queued the request and merged its token; the submit is spinlocks and plain stores (the push into the slot, the GPFIFO
entry, `GP_PUT`, the doorbell): a few microseconds, bounded, never a wait (`CeRtDirUs` measures it). The worker is
signalled at once to poll the copy.

**When (all of them, else the request stays the deferred one: `CeRtDirNo`, `CeRtDirWhy`):** the route decision admitted
the Present (15.2, unchanged); the channel is up and the destination's system copy not marked stale (7 `NotUp`); the
channel has NO copy in flight (1 `ChannelBusy`: a direct copy never queues behind another destination's copy, and only
one direct copy can hold the channel at a time); the destination's descriptor is made (2 `NotPrepared`); both producer
dups are cached under the client table's current generation, a pure lookup (3 `NotCached`: making a dup is RM I/O,
the worker's); the channel's I/O is free (`try_io`, 4 `IoBusy`: while held no dup is remade or given back under the
cached VAs); and, in ONE critical section of the transport lock (`VirtioGpu::ce_direct_dispatch`), the request is the
only one pending for the destination and the destination is taken as `KmdWriter` without waiting (the worker's
`try_begin_present_buffer_write`) (5 `DstBusy`), then the push is submitted (6 `Submit` gives the writer back). A
submitted request is marked dispatched and admitted (SubmitCommand's later admission skips it; a copy that completes
before SubmitCommand leaves its terminal, which the DMA fence then finds ready). Lock order: virtio -> the route's
`STATE` -> the channel's `STATE`, nothing the other way round.

**The deadline starts when the CPU sees the producer finish, not at the submit.** A copy waiting on its acquire waits for
the producer, which may legitimately take long (a heavy frame). The worker checks the boundary of every direct copy it
has not yet seen finish (`VirtioGpu::ce_boundary_seen`: ready, or the stream is dead) on every pass while copies are in
flight, and the first observation starts the copy's `COMPLETE_MS` (100 ms) clock (`ce_route::route_fire_ms`). Unseen,
nothing runs until `DIRECT_CAP_MS` (7 s) after the submit, longer than the RM gate's own expiry (`RmGateMs`, 6 s by
default, after which the boundary is declared ready and the normal deadline runs): the cap only acts with `RmGateMs` 0
or a gate that never fires, and then discharges the copy exactly as a timeout (the fence retires, the destination is
struck, poisoned and leaked, the channel torn down so the acquire cannot hold the GPU, one route strike).

**Head of line.** The channel runs in order: anything submitted behind a direct copy waits for its acquire. Bounded to
nothing for other destinations: a direct submit needs an empty channel, and while a direct copy's producer has not been
seen finishing, the worker's deferred dispatch of ANOTHER destination falls back to its Venus copy (`CeRtWhy` 24
`Blocked`) rather than queue behind it. The same destination cannot have a second copy in flight (`KmdWriter`). If the
direct head is discharged (its own deadline or the cap), every copy behind it is a bystander of the channel's teardown,
each settled exactly once (`Jobs::take_settled` then `take_first` remove what they return; tests).

**Which frame it reads.** The acquire is on THIS Present's own record value (`take_ce_record(boundary)` pairs the record
with this Present's fence; `matches_fence` makes the record's value the fence's). On one producer timeline the values a
queue releases only grow, so `current >= value` holds exactly once frame `value`'s work finished (a later frame's higher
value implies it); an older frame's lower value is released already and never hangs (`ce_route::acquire_releases`).
What GEQ cannot prevent is the source-reuse hazard of every asynchronous copy (24.10.3): if the app renders the NEXT
frame into the same image before this copy ran, the copy reads the newer pixels. The read-ledger claim and the
swap-chain depth bound it exactly as for the deferred copy; the direct copy usually runs EARLIER (right when the
producer releases) than a deferred one, which narrows that window.

**What the destination sees.** `KmdWriter` is taken at the Present and held while the GPU waits for the producer
(longer than with the deferred dispatch, which takes it once the producer finished): a Venus consumer claim of the
destination waits that long; an NVK DWM reads the pages without a claim. The DMA fence still waits for the producer's
boundary and the copy's terminal, unchanged.

**The expected gain, honestly.** `BltDeferUs` is mostly the PRODUCER'S OWN GPU time, which no design removes: the copy
cannot start before the frame is rendered. What direct submission removes is the CPU lag between the producer finishing
and the copy starting: the RM fence's `EventReady` to the DPC, the worker's wake (rounded to the timer) and its dispatch.
Expect `CeRtLag / CeRtDir` (the CPU lag from seeing the producer finish to seeing the copy done) well below the deferred
path's dispatch-to-done, often 0 when the copy finished before the CPU even saw the boundary, and the present-to-fence
retire shorter by roughly the worker hop (a fraction of the 511 us, not all of it).

**Counters** (`ce_route::COUNTERS`): `CeRtDirKnob` (in force), `CeRtDir` (submitted at the Present), `CeRtDirNo` /
`CeRtDirWhy` (not, and the last `DirNo` code above), `CeRtDirUs` (microseconds of those submits, sum), `CeRtLag` (sum,
us). `CeRtDoneUs` of a direct copy runs from its submit, so it includes the producer's time: compare `CeRtLag`, not
`CeRtDoneUs`, between the two modes.

| where | what | the Present |
|---|---|---|
| Present | any `DirNo` condition | queued deferred as with `CeRtDirect` 0 |
| Present | submit refused after the writer was taken | writer given back, queued deferred (6) |
| worker | the producer is slow (a heavy frame) | the copy waits on the GPU, no clock runs; other destinations' deferred dispatches take the Venus copy (24 `Blocked`) |
| worker | the boundary is seen | the 100 ms deadline starts; done: the fence retires on the terminal |
| worker | unseen for 7 s (gate never fired, `RmGateMs` 0) | discharged as its own failure; the channel torn down; bystanders behind it settled once, uncharged |
| channel | breaks under a direct copy | as 15.6 |

**Hardware A/B (same boot, `pnputil /restart-device` between):** the knobs of 15.11 with `RmCopyEngine` 1, then
`reg add ... /v CeRtDirect /t REG_DWORD /d 1 /f`, restart, Heaven windowed 1600x900 for a minute, read the key twice;
`CeRtDirect` 0, restart, the same. Expected with 1: `CeRtDirKnob` 1; `CeRtDir` close to `CeRtRouted` (a few
`CeRtDirNo`: the first frames 2 or 3 while the descriptor and dups are made, 1 when two Presents overlap); `BltDeferUs`
grows only for the deferred remainder (per routed copy close to 0: `BltDeferUs / (CeRtRouted - CeRtDir)` stays what it
was, `BltDeferUs / CeRtRouted` falls); `CeRtDirUs / CeRtDir` a few us; `CeRtLag / CeRtDir` well below the run with 0's
`CeRtDoneUs / CeRtDone` (220 us); every strike, timeout, leak, poison and off counter 0; PresentMon `msBetweenPresents`
and the present-to-fence retire (`PBFnc` latency in the stage trace) equal or shorter. A rising `CeRtWhy` 24 says
another window was held back while a direct copy waited.

Unverified: everything on hardware; that GSP schedules other runlists while the CE channel's acquire waits
(`ACQUIRE_SWITCH_TSG` is set); the admission order when a direct copy completes before SubmitCommand admits its request.

## 16. The NVK and UMD side (as built)

The producer half of 10.2. Nothing here changes what the KMD does with a Present: an older KMD reads the
first 48 / 96 bytes as before, and the record only reaches a KMD that says it reads it.

### 16.1 NVK: `queue_rm_fence_v3` (`patches-windows/0053`)

`helios_icd_interface.h` version 6 appends one entry and two structs:

```c
VkResult (*queue_rm_fence_v3)(VkDevice, VkQueue, VkDeviceMemory memory, VkImage image,
                              uint32_t *fence_handle, uint64_t *value,
                              struct helios_icd_rm_copy *copy);
struct helios_icd_rm_copy { struct helios_icd_rm_semaphore semaphore;   /* 24 bytes */
                            struct helios_icd_rm_source source; };     /* 56 bytes */
```

The two structs are, member for member, `HeliosRmSemaphoreLoc` and `HeliosRmCopySource` of
`helios_rm_fence.h` (the header's own static asserts pin the C sizes, and the protocol test
`the_icd_copy_struct_is_the_record_s_semaphore_and_source` pins the member order against the Rust record).
The ICD does not build the `'HEF3'` record itself: Mesa carries a verbatim copy of `helios_icd_interface.h`
and nothing else of the protocol, so the UMD adds the magic, version, flags and byte count.

Return codes (a change from the proposal in 10.2, which returned an error with the fence still owned):

| result | fence | `*copy` |
|---|---|---|
| `VK_SUCCESS` | the caller's | filled; `semaphore.value == *value` |
| `VK_INCOMPLETE` | the caller's | all zero: the image cannot be described |
| an error | none | all zero (`VK_ERROR_FEATURE_NOT_PRESENT`: no RM fences, wait on the CPU as before) |

A non-negative result therefore always means "the fence is yours", the same rule as `queue_rm_fence`.

How each value is found (the table of 10.2, as implemented):
- the fence: `nvk_queue_rm_fence_sync`, which is `nvk_queue_rm_fence` that also returns the present timeline's
  `vk_sync`. The source is described before the fence is made, so a refusal changes nothing.
- the semaphore: `nvkmd_rm_rmfence_semaphore` finds the timeline under `rmfence_mutex` and returns
  `lib->root(client)`, `nvkmd_rm_mem(rf->mem)->h_memory` and `index * entry_size_B`. That is the same entry
  `rmfence_tl_locked` gave the semaphore surface.
- the source: `helios_image_layout(image, formats = true)`, one plane only. The memory must be the image's
  dedicated memory, and the handles come from `nvkmd_rm_mem_rm_handles` (the device's root client and
  `h_memory`). `size` is `mem->size_B`.
- refusals (`VK_INCOMPLETE`): no image or memory, not dedicated, `is_compressed`, a layout
  `helios_image_layout` refuses, two planes, and a block-linear image whose PTE kind differs from the kind
  its modifier names (bits 12..19). The last one is the open item of 10.3 seen from the producer side. nil
  picks `GENERIC_MEMORY` (0x06) for every uncompressed GB20x image and the modifier family names 0x06, so
  today it never fires. If it ever did, the KMD would map the source with a kind other than NVK's.

### 16.2 UMD: the 168-byte `HERF` and the 192-byte `HEPR`

- **Capability.** `HELIOS_NVRM_CAP_RM_FENCE_TAIL_V3` = `QUERY_CAPS.supported_ops` bit 37 (Rust
  `protocol/src/rm_fence_v3.rs`, C `helios_rm_fence.h` and `rmclient/src/helios_nvrm_escape.h`). This is the
  bit 10.2 left "to be assigned in M3c". The KMD sets it when it parses the record, whatever `RmCopyEngine`
  says: the record is an input, and the route decision stays the KMD's. The UMD reads it with the cached
  per-process `QUERY_CAPS` that the flush gate already asks (`scanout_acquire::nvrm_supported_ops`).
- **When the record is filled.** `nvk_present_frame` -> `nvk_marker_fence` (`umd/src/forward/present.rs`),
  only on the composed path that already sends an RM fence in the marker (carrier (b), `NvkRmFencePresent`).
  All of these must hold: bit 37, the knob `NvkRmCopyRecord` (default 1, `HELIOS_NVK_RM_COPY_RECORD`), and a
  shown resource that is the WDDM present's source. When DXGI hands a destination resource, the UMD copies
  into it itself, the KMD's Blt source is not what NVK rendered, and no record is sent. Otherwise the fence
  goes alone through `nvk_present_fence`, exactly as before.
- **The bridge.** `HeliosDxvkDevice::nvk_present_fence_v3` (`umd/bridge/dxvk_bridge.cpp`) takes DXVK's
  submission lock like `queue_rm_fence` and resolves the texture's `VkImage`/`VkDeviceMemory`. An image at a
  non-zero memory offset is passed as null, so NVK answers `VK_INCOMPLETE`. An NVK without the entry gets
  the plain `queue_rm_fence`, with return code 2 = fence only.
- **The check before sending.** `helios_protocol::producer_record` builds the record from the 80 bytes and
  applies every rule the KMD applies (`HeliosRmFenceTailV3::validate`, `matches_fence`). A record the KMD
  would refuse is never sent: the frame goes with the 48 / 96-byte fence form, and the reason is counted.
- **The wire.** `PresentStreamCorrelation.rm_copy` carries the record. The `HERF` becomes
  `HeliosPresentRefreshCmdRmCopy` (168 bytes, the on-scanout slot 48..72 zero) and the `HEPR` becomes
  `HeliosPresentRenderCmdRmCopy` (192 bytes). `command_length_and_label` and the writer both decide through
  `rm_copy_record`, so the length and the bytes cannot disagree. The record never travels with the on-scanout
  tag, because an on-scanout frame is not copied.
- **Log lines.** `NVK present: copy-engine record: semaphore ... source ...` (the first record, every field),
  `copy-engine record not sent: <why>` (first 8, then every 4096th), `NVK described no copy source`, and
  the record / without / refused counts on the every-4096th `WDDM presents carry an RM fence` line.

### 16.3 Tests and what is unverified

- Host: the protocol crate (`cargo test` in `guest/windows/protocol`): the carriers place the record where
  the KMD reads it, the ICD struct mirror, the capability bit is free, and `producer_record`'s refusals. The
  NVK series applies in order and cross-builds (`build-windows.sh`, x86_64). The Windows test tool builds
  (`guest/nvk-rm/windows/build-tests.sh`).
- Guest, not run yet: `vk_rmfence_test.exe` now ends with a `copy record` check. It fills a dedicated
  1920x1080 image, calls `queue_rm_fence_v3`, checks the record against the KMD's rules (value equal to the
  fence's, 8-aligned offset in the 4 KiB surface, handles nonzero, the image inside its memory, the GB20x
  family), waits for the fence, and checks the null-image case (`VK_INCOMPLETE`, record zero). It exits 2 on
  a fault.
- The UMD cannot be type-checked on the Linux host (WDK bindgen, clang-cl). Its first compile is the
  Windows build.
- End to end (M3c): the record parsed by a KMD that sets bit 37, the `h_client` check against the process's
  recorded clients, the dup of `h_memory` from the KMD client, and the mapping kind (10.3).
