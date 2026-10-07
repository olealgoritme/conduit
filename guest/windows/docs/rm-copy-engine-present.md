# The windowed Present copy on an RM copy-engine channel: feasibility findings

Status: M0 (this document). Research only: no code exists for any part of this. Every claim cites the file or patch it comes
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
- Not built: the block-linear source (`bl`) and a compressed source (unknown 1 of section 6). Both still need a later tool.
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
