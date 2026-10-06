# The flush gate (HEFL): a D3D11 flush whose WDDM fence means "the GPU work is done"

Status: KMD side implemented (`kmd/flush-completion`), ABI in `protocol/src/flush_gate.rs` and
`protocol/include/helios_flush_gate.h`, pure rules in `kmd_logic/src/flush_gate.rs` (host-tested).
Nothing here is built or run on Windows. The UMD side is written by another session; section 4
is the contract it must meet.

## 1. The problem and where it comes from

Cross-process keyed-mutex hand-offs misorder (`d3d11_share` keyed-load test, 18 of 20 rounds
stale on Venus; NVK the same). Process A queues work, calls `ReleaseSync(1)`; process B returns
from `AcquireSync(1)` and reads stale texels.

* The D3D11 runtime flushes (`pfnFlush`) and then releases the key through dxgkrnl
  (`D3DKMTReleaseKeyedMutex(2)`: `Key` and a `FenceValue` "new fence value to use for GPU sync
  object", `icd/win-build/wdk-include/d3dkmthk.h:4640`, `:4708`). dxgkrnl can only order that
  release against the DMA buffers the device has submitted (their `SubmissionFenceId`).
* **The D3D11 UMD submits no DMA buffer at a flush.** `flush` is `context.Flush()` and nothing
  else (`umd/src/forward/transfer.rs:296`). The only `pfnRenderCb` calls of the D3D11 UMD are
  presents (`umd/src/forward/present.rs:809-975`). The actual work runs on DXVK's submission
  thread into the Venus ring (ICD escapes: `HELIOS_ESCAPE_SUBMIT_VENUS`, `enqueue_submit_inner`,
  `virtio/gpu/mod.rs`) or NVK's RM channel. So the fence dxgkrnl waits on is an old one, or none.
* Even a DMA buffer would not be enough on its own: `Flush()` returns while the frame's work may
  still be queued on DXVK's CS and submission threads (`umd/src/forward/present.rs:1496-1513`,
  the comment above the ordering gate), so "everything enqueued at SubmitCommand" misses it.

## 2. KEY QUESTION: can the UMD reuse the existing HE12 record with no KMD change?

Short answer: **it would be accepted, but it is the wrong carrier for a D3D11 flush**, so a small
KMD change (a new record, `HEFL`) was made. Evidence, in the order the decision depends on it:

1. **Accepted: yes.** `dxgkddi_render` identifies HE12 by magic only (`submit_command.rs:1792`)
   and resolves the stream point with the context's `creator_process` (`present_stream_marker_
   boundary`, `virtio/gpu/mod.rs:5956`); nothing asks whether the context is a D3D12 queue. Every
   context gets the same 112-byte private data (`device.rs:488`, `present_packet.rs:36`), and the
   execution record lives in it at +88. A D3D11 context can carry HE12 v3 `(ctx, value, cookie)`
   or v4 FENCE today.
2. **A D3D11 device has no execution stream until an HE12 binds one.** `execution_stream` is a
   per-`ContextContext` atomic (`device.rs`), bound by `bind_execution_stream`
   (`submit_command.rs:1890`) under `advances_context`
   (`kmd_logic/src/execution_completion.rs:18`): same stream, strictly increasing value, never a
   switch. It is sticky for the context's life. If the ICD re-registers its stream (a new handle) while the
   device lives, every later HE12 `Render` fails with `STATUS_INVALID_PARAMETER`; a D3D11
   `pfnFlush` must not fail.
3. **Liveness: HE12 is unbounded, and a flush gate runs on every flush.** An execution wait is
   exempt from the `WddmHeadMs` rebase (`rebase_blocked_head`, `virtio/gpu/mod.rs:7877`, first
   statement; `take_one_ready_wddm` `:7707`, top) and from dead-stream discharge
   (`discharge_dead_present_stream_waits` `:5883` skips entries with `execution`). The WDDM FIFO
   is adapter-global and strictly head-of-line, so one HE12 whose point never retires (a UMD error
   between reserving the point and submitting the tagged batch; device lost) pins every fence of
   every process, DWM included, until a TDR. D3D12 accepts that because an early runtime fence
   signal corrupts rendering. A flush gate would meet it on every flush of every shared-resource
   device.
4. **Cannot express "legacy wire rung" or "nothing new".** v3 refuses `value == 0`; a repeated
   value is refused by `advances_context`.
5. **Conflation.** HE12 packets are scoped as D3D12 by identity (`note_wddm_submission(d3d12 =
   true)`, `WddmHoldMs` hold, `D12*` counters).

`HERF` / `HEPR` carry the same marker without these problems but are present-only: HERF arms a
scanout refresh and both stash the marker for the Present that follows (an orphan stash would be
claimed by the next unrelated Present).

So `HEFL` is the HERF/HEPR **advisory** semantic (never fails the Render, degrades to the legacy
rule, always takes a fence handle) on the **present-style** boundary (`PresentSubmissionPrivate::
stream_boundary`), with none of their side effects.

## 3. Decision list

1. **One 48-byte record, `HEFL`** (`HeliosFlushGateCmd`), the whole command of one tiny
   `pfnRenderCb`, no allocations, no patches. Variants by flags: `STREAM` (Venus point),
   `RM_FENCE` (the NVK carrier of `rm-fence-marker.md`, same fence object and ownership rules),
   none (the WIRE rung).
2. **Boundary = the existing tagged stream boundary**, merged into the DMA buffer's
   `PresentSubmissionPrivate` at Render (`flush_gate_record`, `submit_command.rs:1649`), exactly
   where a Present puts its marker (`display.rs` end of `dxgkddi_present`). Everything downstream
   is unchanged and already measured: `note_wddm_submission` takes it as `stream_boundary`
   (`virtio/gpu/mod.rs:7265`; with `PresentWmk` on, watermark 0, i.e. the stream point is the only
   dependency, no over-wait on other processes' work), `scanout_boundary_ready` evaluates it, the
   used-ring retirement of the stream's tag or the `nvrm_events` DPC of the RM fence re-evaluates
   the FIFO head.
3. **Bounded for the stream and RM-fence flavours only.** HEFL is rebasable (HE12 is not:
   `rebase_blocked_head` returns false only when the head has an `execution` wait, which HEFL
   never writes, it does not use the +88 record). A stream or RM-fence gate whose point has not
   retired after `WddmHeadMs` (250 ms default) at the head is rebased (`rebase_blocked_head`,
   counted `WfBReb*`): its stream/gate dependency is swapped for the wire prefix AT REBASE TIME
   (`next_wire_fence` then, `mod.rs` rebase), a dead stream or an RM gate purge discharges it.
   What the swap means: if the flush's tagged work is already enqueued the prefix covers it and
   the fence retires when everything enqueued so far has (which can be LONGER than the point
   itself, across all processes: a deep backlog of another process, DWM's, lengthens it); if
   the work is not yet enqueued the prefix does not cover it and the fence is released early,
   one stale read for a keyed mutex, counted. (`WddmHeadMs=0` makes it unbounded.)

   The **wire rung** (`flags = 0`) and **every degraded record** are NOT bounded by the rebase:
   their dependency is a wire fence, and the wire arm of `take_one_ready_wddm` returns before
   any rebase (nothing to rebase a wire dependency onto). They wait for real GPU completion of
   every transport entry enqueued before the Render, across ALL processes, however long it
   takes; a host that retires nothing is a TDR, as for every submission. The head-of-line cost
   is therefore the largest on exactly the rung that is the fallback.
4. **Advisory.** `Render` never fails for the boundary: unknown flags, an incomplete stream tail,
   a stream that is not this process's or not live, a refused fence, a boundary the buffer
   replaced with an older record's wait, or no private-data room leave the packet on the wire
   rung (GPU completion included, `dma_gpu_fence` default on) and bump `FlGDeg`. A fence handle in
   the tail is the KMD's afterwards, attached or not (`take_fence_tail`), for every record of
   magic `HEFL` and 48 bytes: a version this KMD does not know is not resolved, but its handle is
   taken (`FlGVer`; a future layout must keep the tail at +32, or its handle leaks as it would on
   an older KMD). The one exception is a Render with no live context (no adapter or process to
   claim the handle for; unreachable from dxgkrnl): nothing is taken there.
   Ownership is claimed only for a fence of the Render's own process, so a guess about a
   foreign layout cannot close anything else.
4a. **The wire rung is stamped, not inherited.** Nothing consumes the Present prefix of the DMA
   buffer's private data (`PresentSubmissionPrivate::decode` only peeks) and dxgkrnl recycles
   those buffers. A packet that ends with no boundary of its own (wire rung, degrade, merge
   error, a boundary the merge did not keep) therefore gets an explicit wire fence written into
   the record at Render: the last fence of this transport generation, `next_wire_fence - 1`
   (`flush_gate::wire_floor`; skipped when the generation has issued nothing). Without it a stale
   `gpu_fence_id` would become the watermark (wait only up to that old id) and a stale live
   same-stream boundary would select the exact-present-watermark arm (watermark 0: no wire wait).
   `note_wddm_submission` evaluates the `gpu_completion_fence` arm first and the merge keeps the
   larger id, so the stamp wins over both. Its watermark is "every transport entry enqueued
   before the Render" (not before `SubmitCommand`): the UMD's flush has reached the transport by
   then (section 4), later work is not this flush's. A merged boundary that the record kept is
   not touched (a stream/RM-fence gate keeps watermark 0 and no over-wait). A recycled record of
   a different handle with a real wait keeps its wait and the new boundary is dropped by the
   merge: that is detected (`flush_gate::boundary_kept`), the packet is stamped and counted
   `FlGDeg`.
4b. **The gate does not move the present-marker calibration.** Its stream resolution
   (`flush_stream_marker_boundary`) and merges (`merge_flush_boundary`, `merge_flush_fence`) bump
   none of `PRESENT_STREAM_MARKERS`, `PsMkAhd` / `PsMkAhdHi` / `PsMkCpl` (read against the present
   path's pipeline depth), `PRESENT_STREAM_REJECTS`, `PrBndDrop`, `PRESENT_MARKER_WRITES` /
   `PRESENT_MARKER_LAST_*` (also the gate of the private-data diagnostic scan). A flush point is a
   different producer pattern; the gate has its own counters (section 6).
5. **No sticky state.** No `bind_execution_stream`, no stash, no scanout refresh.
6. **Non-blocking.** Nothing waits in `pfnFlush` or in the KMD: the WDDM fence is withheld in the
   FIFO and retired from the completion DPC. dxgkrnl waits only if someone waits on the fence,
   i.e. the key release.
7. **Capabilities.** `HELIOS_SCANOUT_CAP_FLUSH_GATE` (bit 5 of the `MAP_READ_LEDGER` PROBE reply)
   and `HELIOS_NVRM_CAP_FLUSH_GATE` (bit 34 of NVRM `QUERY_CAPS.supported_ops`, same
   preconditions as bit 33). An older KMD copies the unknown bytes, returns success, gates
   nothing and does NOT take the handle, so the UMD gates on the bit.

### What the KMD can compute itself, and what only the UMD can send

The coordinator asked whether the KMD could use "its own latest submit point of the context" so
the UMD only marks the flush.

* **The KMD cannot see a flush.** No DDI reaches the KMD at `pfnFlush`, and dxgkrnl tells the
  miniport nothing about keyed mutexes (there is no keyed-mutex DDI in the DDI table; the
  keyed mutex is a dxgkrnl object created by `D3DKMTCreateKeyedMutex(2)`, and the shared-resource
  bit the KMD does see is `DXGK_CREATEALLOCATIONFLAGS.CreateShared` at `CreateAllocation`, which
  carries no keyed-mutex bit). `DeviceContext` keeps no per-device allocation state
  (`device.rs:20`). So the **UMD must submit a packet at the flush**; there is nothing for the
  KMD to defer otherwise. That packet is the minimum "marker": a `HEFL` with `flags = 0`.
* **What the KMD computes for that packet with no further input is the WIRE rung**: the fence
  is gated on every transport entry enqueued before the Render (`next_wire_fence - 1`, stamped
  into the packet's private record, decision 4a; GPU completion included). That is exact for
  work that already reached the transport, and an over-wait on other processes' work (head of
  line, and not bounded by the rebase, decision 3), so it is a fallback and a first-bring-up
  rung, not the target.
* **What the KMD cannot compute is the not-yet-submitted work.** Flush returns before DXVK's
  submission thread has submitted. Whatever counter the KMD keeps (per-context last fence of
  `enqueue_submit_inner`, the stream's `submitted_value`) is behind it. Two ways to close the gap:
  (A) the UMD waits for its own submission thread (not for the GPU) before the packet, then the
  wire rung is exact; a CPU wait on a thread the present already waits for (`present_frame_gate`
  SUBMITTED mode), but a CPU wait per gated flush; (B) the UMD sends a stream point that is
  *ahead* of the submission (the normal state for present markers, `PsMkAhd`), non-blocking and
  exact. (B) is the design; (A) is the rung that needs no producer change.
* **Why `value == 0` does not mean "KMD, use your latest"**: in the stream namespace 0 already
  means "already complete" (`present_stream_marker_boundary`, `PsMkCpl`; NVK's CPU-complete
  producers). Overloading it would turn a CPU-complete marker into a wait. The "KMD, use your
  latest" meaning is `flags = 0`.
* **A cheaper KMD-computed exact wait is possible but not implemented**: record the last async
  fence per process (owner token -> `creator_process`, one write per ICD submit in
  `submit_venus_async_inner`) and gate on `async_exact_retired` of it, which avoids the head of
  line behind other processes. It still needs (A)'s wait for work not yet submitted, and it
  touches the hottest submit path. Left for when `FlGWire` shows the over-wait matters.

## 4. The exact ABI the UMD must send

```
HeliosFlushGateCmd, 48 bytes (little endian)
  0  u32 magic    = 0x4C464548 ('HEFL')
  4  u32 version  = 1
  8  u32 flags    bit0 STREAM, bit1 RM_FENCE; 0 = WIRE
 12  u32 ctx_id   STREAM: Venus context of the producer stream
 16  u32 value    STREAM: the point (0 = "already complete", the stream must be live)
 20  u32 reserved = 0
 24  u64 cookie   STREAM: the stream's registration cookie
 32  HeliosRmFenceTail { u32 rm_fence_handle; u32 flags (FENCE=1); u64 rm_fence_value (diag) }
```

* One `pfnRenderCb` from `pfnFlush`, on the device's own `hContext` (the one `present.rs` uses),
  `CommandLength = 48`, `NumAllocations = 0`, `NumPatchLocations = 0`, command bytes = the record.
  dxgkrnl must see this as the device's last submission before the runtime calls
  `D3DKMTReleaseKeyedMutex`; both run on the flushing thread, so the render callback must be
  synchronous inside `pfnFlush`.
* The UMD cannot know which flush precedes a release (the DDI has no keyed-mutex entry and
  the runtime's `MISC_SHARED_KEYEDMUTEX` never reaches the UMD: measured v312, API misc 0x900
  arrives as DDI misc 0x2). So: **every flush of a device that holds a cross-process shared
  resource** (created or opened `MISC_SHARED`, not `BIND_PRESENT`), **and only when work was
  recorded since the previous gate** (`FlushFlags UNLESS_NO_COMMANDS`, or DXVK's own "nothing
  pending"). An empty flush needs no packet: the previous gate's fence (FIFO order) is the
  device's last fence.
* Venus (`flags = STREAM`): `(ctx_id, value, cookie)` of the process's registered producer
  stream (the same tuple `publish_present_order` returns for a present). Required of the
  producer: (i) `value` is carried by a tagged Venus submission on the queue timeline AFTER all
  work recorded before the flush (so its used-ring retirement implies that work finished);
  (ii) `value` is not below the stream's last tag and not skipped over an unsubmitted reserved
  batch (the KMD's tag rule, `prepare_present_stream_tag`, `virtio/gpu/mod.rs:6391`: `value >
  submitted_value`, and equal to a pending claim). The bridge today offers the point only through
  `publish_present_order(resource)`; a resource-less "publish a flush point" entry in the DXVK
  bridge is the missing producer piece (not KMD). Guard: never send a point whose tagged
  submission may not follow (an error between reserving and submitting); fall back to the wire
  rung instead (a lost point costs a 250 ms rebase, not a hang).
* NVK (`flags = RM_FENCE`): end the flush batch with an RM semaphore release, create a
  `SEMSURF_FENCE_CREATE` fence for that value right before the packet (`rm-fence-marker.md`
  "The fence object"), put the handle in the tail with `flags = FENCE`. Never `Close`,
  `EVENT_REGISTER` or reuse the handle afterwards, whatever the `Render` returned. Limits as for
  the other (b) carriers: 128 unretired points per gate, 512 KMD-held fences.
* No boundary available (`flags = 0`): first make sure the flush's work has reached the
  transport (wait for the submission thread), then send the packet.
* Gate on the capability bit of the carrier used (decision 7); a refused/failed `pfnRenderCb`
  must not fail `pfnFlush`.

## 5. Failure modes

| situation | outcome | counted |
|---|---|---|
| ring never retires the point (host hang, lost tag), stream / RM-fence flavour | head blocks; after `WddmHeadMs` rebased onto the wire prefix at that moment (itself non-rebasable: a host that retires nothing is a TDR, as for every submission) | `WfBReb`, `WfBRebS` |
| wire rung or degraded record, GPU backlog long (any process's) | head blocks until every transport entry enqueued before the Render retired; NO rebase (the wire arm returns before it), only the TDR bounds it | `WfBWire` (existing, adapter-wide) |
| stream unregistered / process exits before the point retires | `discharge_dead_present_stream_waits` clears the wait (watermark 0 with `PresentWmk`), the fence completes: a cancellation, not success | none of its own (the discharge is silent for a Venus stream; `RmGCan` for a gate) |
| RM fence never fires | host times it out at 5 s (`-ETIMEDOUT`), fire with error retires the point; the 250 ms rebase applies first | `RmGErr`, `WfBReb` |
| RM gate process exit | gate purge discharges waits that named it | `RmGCan` |
| device reset / TDR / `StopDevice` | pending FIFO dropped by `preempt_flush`/`abandon_pending_submissions`; dxgkrnl resubmits; the private record is not consumed so the replay re-decodes it: a live point is waited for again, a dead one degrades | existing `ABANDONED_FENCES` |
| record unusable (unknown flags, incomplete stream, not this process's, dead, no room, boundary replaced by a recycled record's wait) | wire rung stamped into the record, fence handle taken and closed | `FlGDeg`, `FlGFlr`, `RmGRef` |
| `HEFL` of a version this KMD does not know | not resolved; fence handle of the process taken and closed; wire rung stamped | `FlGVer`, `FlGFlr` |
| recycled private-data prefix of an earlier Present on the context | overwritten for the wire rung (stamp); a kept boundary is unaffected; a stale `blt_token` is not scrubbed (pre-existing, only exact live transactions attach, `can_attach_dependency`) | none |
| older KMD | silent: nothing gated, handle not taken. Gate on the capability | none |
| head-of-line cost | every fence of every process waits behind a gated flush: for a stream / RM-fence flavour until its point retires or the 250 ms rebase (which then waits for the wire prefix of that moment, and can be longer than the point when other processes have a deep backlog: DWM); for the wire rung and degraded records until the GPU work of everything enqueued before it retired, unbounded | `WfBStrm`, `WfBReb`, `WfBWire` (existing, adapter-wide) |

## 6. Counters (published beside the `D12*` set in `record_present_handoff_telemetry`, PASSIVE, on the present edge: a session with no present publishes nothing until one runs)

`FlGRec` valid records, `FlGStrm` stream boundaries carried, `FlGFnc` RM fence boundaries carried,
`FlGWire` records that retire by the wire rung, `FlGDeg` records that asked for a boundary or
were malformed and did not get it. `FlGRec = FlGStrm + FlGFnc + FlGWire`; `FlGDeg` overlays.
Healthy: `FlGDeg = 0` and, on Venus with the producer in place, `FlGWire = 0`.
`FlGFlr` records stamped with an explicit wire fence (every `FlGWire` record, when the transport
generation had issued a fence; a gap means a Render with no private-data room or a fresh
transport). `FlGVer` records of magic `HEFL` and 48 bytes with a version other than 1 (expected
zero until a newer UMD ships). None of these is a present-marker counter, and the gate moves none
of those (decision 4b).

## 7. Locks and IRQL

`flush_gate_record` runs in `DxgkDdiRender` (PASSIVE, as the other fence tails). Stream resolution
takes `virtio_lock` via `AdapterContext::with_virtio` (scan of the fixed stream table, no
allocation); RM attach and take are the existing (b) routines (`virtio_lock`, fixed arrays) and
the PASSIVE `fence_taken` pass. No `wddm_notify` lock is taken at Render: the merge writes only
the DMA buffer's private data. Completion is the existing DPC path (stream retirement or
`nvrm_events`), which allocates nothing and signals DMA_COMPLETED under the notify lock as for any
present.

## 8. Untested (needs the Windows guest)

* That dxgkrnl's keyed-mutex release (and the sync-object signal the runtime queues beside it)
  is ordered after a packet submitted from `pfnFlush` on the device's `hContext`, and which
  contexts it considers. Check with GPUView/ETW (`DmaPacket` vs `ReleaseKeyedMutex`).
* The D3D11 UMD half, the resource-less flush point in the DXVK bridge, and NVK's fence creation.
* That the offset the stamp (and the boundary merge) writes at, the start of the Render's private
  data, is the offset `SubmitCommand` decodes for the same DMA buffer (the assumption every
  Present marker already makes, `decode_present_fence`), and that a recycled buffer really
  presents an earlier Present's prefix there; the stamp makes the wire rung independent of what
  it holds, the merged-boundary flavours are not scrubbed of a stale `gpu_fence_id` (it only adds
  a wait up to that old id: it replaces the exact-present-watermark relaxation's watermark 0).
* The rebase of a stream / RM-fence flush gate behind a deep backlog of another process (DWM):
  whether the swap to the wire prefix lengthens the wait in practice.
* Head-of-line cost with DWM among the gated devices (it holds shared surfaces).
* The `d3d11_share` keyed-load test (400 copies, 20 rounds) on v313 + this change, Venus and NVK.

## 9. Why the gate does not order the key release (investigation, `kmd/flush-gate-why`, diagnostics `kmd/flush-gate-why2`)

Status: the findings are from reading the code. The diagnostics are WRITTEN (pure part host-tested,
`kmd_render` part type-checked only against a stub harness, see "What was built"); nothing has been
built as a driver or run in the guest. Context: the `HEFL` packet is off by default, and the peer reports
that the keyed mutex now passes with an acquirer-side wait, which is finding 1 below. The diagnostics are
therefore evidence for or against that reading, not a fix.

### Finding (ranked)

1. **Most likely, structural: the acquirer's work is not ordered, whatever the releaser's fence does.**
   `d3dkmthk.h` (`D3DKMT_RELEASEKEYEDMUTEX.FenceValue` "new fence value to use for GPU sync object",
   `D3DKMT_ACQUIREKEYEDMUTEX.FenceValue` "current fence value of the GPU sync object") says dxgkrnl's
   keyed mutex orders GPU work through a GPU sync object: the acquirer's context queue waits on it.
   That orders only the acquirer's DMA buffers. B's reads run through DXVK into the Venus ring / NVK's
   RM channel (escapes), never in a DMA buffer, so nothing holds them; B's only DMA packet is its own
   `HEFL`, queued after the work was already forwarded and never waited for by the UMD. `AcquireSync`
   returns on the CPU as soon as A calls the release, so B reads while A's 400 copies run. This explains
   "same on Venus and NVK", "unchanged with the gate active" and "A always sees B's writes" (B's write is
   small and long finished). It is also the gap `shared-surfaces.md` section 4 named ("the acquirer still
   issues NVK work dxgkrnl does not see: it needs a GPU wait") and the v1 CPU rule hid: "the acquirer needs
   nothing" held only while the releaser waited on the CPU. The gate removed that wait.
2. (a) dxgkrnl eliding the packet: unlikely. `dxgkddi_render` copies the 48 bytes into `pDmaBuffer` and
   advances it (`submit_command.rs` end of `dxgkddi_render`), so the buffer is non-empty; the counters
   `FlGRec` = 95 prove Render runs. Whether each packet reaches SubmitCommand is UNMEASURED (no counter).
3. (b) wrong context: unlikely. The UMD creates one context per device (`device_funcs.rs` CreateDevice) and
   `send_flush_gate` uses it, like presents.
4. (c) early retire: not found in the code. `take_one_ready_wddm` retires a stream/RM boundary only when
   `present_stream_slot_ready` (`retired >= value`); the host retires a fenced SUBMIT_3D only after the
   renderer signals its fence (`host/backend/device/src/venus/fence.rs`). Unverified traps: a stream point
   of value 0 ("already complete"), the 250 ms rebase, and the assumption of section 8 that SubmitCommand
   reads the private-data offset Render wrote (`FlGStrm` counts at Render, not at SubmitCommand).
5. (d) async submission order of the runtime's signal vs our Render: cannot be seen from the KMD.

### Fix options (the knob exists as a diagnostic, the UMD fix does not)

* Cheapest and testable without a UMD change: the registry knob `FlGSyncMs` (built, see below, default off): the
  `HEFL` Render waits (bounded, clamped by `clamp_sync_ms`) until its boundary retired
  (`VirtioGpu::flush_gate_ready`, read-only on `scanout_boundary_ready` / `async_retired_up_to`), so the
  key release follows completion on the CPU. If `d3d11_share keyed-load` passes with it, finding 1 is
  confirmed. Cost: a GPU-latency stall per flush of every shared-resource device (DWM too), hence a knob.
* Proper fix, UMD: the acquirer must not forward work before its own context's pending GPU waits are
  satisfied, e.g. send a barrier `HEFL` first and wait for its WDDM fence before releasing the batch.
  Not in this session's scope (umd/ is read-only).

### What was built (`kmd/flush-gate-why2`)

DIAGNOSTIC ONLY. With `FlGSyncMs` at 0 (the default) the driver's behaviour is unchanged: every addition
records into atomics or a per-context stash and no return value or branch of the old code depends on it.

Pure (`kmd_logic/src/flush_trace.rs`, host-tested): the 64-event `Ring`, `submit_matches`, `lag_us`,
`verdict`, `clamp_sync_ms` (cap `SYNC_MS_MAX` = 2000 ms), plus `PendingFlush` (what a Render leaves for its
SubmitCommand), `Outstanding` (16-slot atomics table of queued HEFL fences with their submit stamp, so lag is
exact even when the ring wrapped), `gate_bits` and `pack_event`.

KMD (`kmd_render/src/ddi/flush_trace.rs`, wired from `submit_command.rs`, `interrupt.rs`, `device.rs`,
`virtio/gpu/mod.rs`):

| Point | Where | Records |
|---|---|---|
| RENDER | end of `flush_gate_record` (PASSIVE) | event with the gate kind (STREAM / FENCE / WIRE flags, plus DEGRADED, STAMPED, ZERO_POINT, BATCHED), `aux` = the stream point asked, `boundary` = the merged boundary or the stamped wire floor; leaves a `PendingFlush` on the context |
| SUBMIT | both SubmitCommand DDIs, inside `note_and_maybe_signal` under the notify lock | takes the context's pending record; compares the decoded boundary with the merged one (`submit_matches`); event with the gate bits, MATCH, EMPTY (no private-data record found), IMMEDIATE (fence satisfiable now); a queued fence is added to `Outstanding` |
| RETIRE | completion DPC after a successful `DMA_COMPLETED` (`WddmReady::rebased` read-only accessor), or SubmitCommand itself for an immediate fence | event with the gate bits, `aux` = microseconds since the SUBMIT (exact, from `Outstanding`), REBASED, NO_SUBMIT, AT_SUBMIT |
| (abandon) | `abandon_pending_submissions` | `Outstanding` cleared (`FlGAbn` counts the dropped fences) |
| (overlap) | `enqueue_submit_inner`, one relaxed load | `FlGUnord` while a queued HEFL fence is outstanding |

Pairing a Render with its SubmitCommand is per context and one deep (dxgkrnl issues them in order). Two
Renders before one SubmitCommand count `FlGBat`. A HEFL Render whose DMA buffer dxgkrnl never submits would
hand its record to the NEXT submit of that context (a present's): bounded to one record, and visible as
`FlGSub` < `FlGRec` plus a mismatch.

### How to read the counters (registry values under the service key, PASSIVE mirror on the present edge)

Published only once a HEFL Render has been seen or `FlGSyncMs` is set; an absent `FlG*` value reads as zero.
New values (the older `FlGRec FlGStrm FlGFnc FlGWire FlGDeg FlGFlr FlGVer` are unchanged):

* `FlGSub` DMA buffers that reached SubmitCommand with a HEFL record. Compare with `FlGRec`: fewer means
  dxgkrnl dropped or never submitted some packets (finding 2). `FlGBat` explains part of a gap.
* `FlGMat` / `FlGMis` the decoded boundary was / was not the one the Render merged. `FlGEmpty` of the
  mismatches, those where SubmitCommand found no record at all: the private data it reads is not the range
  Render wrote (the section 8 assumption).
* `FlGImm` fences completed inside SubmitCommand, never queued: the gate held nothing. `FlGImm` close
  to `FlGSub` means the gate cannot order anything.
* `FlGRet` tracked fences delivered (immediate ones included); `FlGReb` of them released by the `WddmHeadMs`
  rebase, not by the point.
* `FlGLagAvg` / `FlGLagMax` microseconds from SubmitCommand to `DMA_COMPLETED` of the QUEUED fences. A few
  microseconds is a wait-free pickup; the copies of the failing keyed test take milliseconds, so a gate that
  really waits reads in the milliseconds.
* `FlGUnord` Venus transport submissions that entered while a queued HEFL fence was outstanding. Adapter
  wide: DWM's and the releaser's own next frame count too. Evidence that nothing holds other work behind the
  gate, not of one consumer.
* `FlGTblFull` queued fences the 16-slot table had no room for (not tracked, so under-counted in `FlGRet`).
* `FlGVerdict` the pure reduction (`flush_trace::Verdict`): 0 no data, 1 not submitted, 2 boundary lost,
  3 retired at submit, 4 rebased, 5 consumer unordered (every step honest AND other work entered while the
  gate was open: finding 1), 6 gate honest (the cause is not in what the KMD observes: read the events).
* `FlGEvSeq` the ring cursor; `FlGEv0` .. `FlGEv7` the newest eight events, `FlGEv0` the newest, each packed
  by `pack_event`: bits 30..31 kind (1 RENDER, 2 SUBMIT, 3 RETIRE, 0 empty), bits 22..29 flags, bits 0..21
  `aux` (microseconds for SUBMIT / RETIRE since the Render / SUBMIT, the stream point for RENDER; clipped to
  4 194 303). Flags of RENDER: 1 STREAM, 2 FENCE, 4 WIRE, 8 DEGRADED, 16 STAMPED, 32 ZERO_POINT, 64 BATCHED.
  Flags of SUBMIT: 1 MATCH, 2 IMMEDIATE, 4 EMPTY, and the gate kind 16 STREAM / 32 FENCE / 64 WIRE. Flags
  of RETIRE: 1 REBASED, 2 NO_SUBMIT, 4 AT_SUBMIT, and the gate kind as for SUBMIT. The full 64-event ring
  (with fence ids, boundaries, stamps) is the static `RING` in `ddi/flush_trace.rs`, readable from a debugger.
* `FlGSyncEff` the `FlGSyncMs` value in force after clamping (0 = off). Shows whether the knob took.

### The `FlGSyncMs` knob (default 0 = off)

Service-key REG_DWORD `FlGSyncMs`, snapshotted at transport init (`pnputil /restart-device` applies it),
clamped in code to 2000 ms. When nonzero a HEFL Render that carries a boundary (stream point / RM fence) or
a wire floor waits, at the end of `flush_gate_record`, until that wait has retired
(`VirtioGpu::flush_gate_ready`), then returns, so the runtime's key release follows the GPU completion on
the CPU. Rules it was built to: PASSIVE only (`DxgkDdiRender`); no lock across the wait (each poll takes
`virtio_lock` for one read-only readiness test and drops it before sleeping, the same way `ctrl::wait_block`
does, including draining the used ring so a lost interrupt cannot pass for a timeout); bounded (the
deadline, plus one timer tick of overshoot: a 1 ms `KeDelayExecutionThread` sleeps up to ~15.6 ms); a dead
stream or a down transport ends the wait at once. With 0 nothing runs, not even the IRQL check.

Reading it: `FlGSyncWt` waits that actually blocked (0 means the boundary was always already ready, or the
knob did not take: check `FlGSyncEff`), `FlGSyncTmo` those that hit the bound, `FlGSyncMaxUs` the longest.
Run the failing keyed-mutex test with the knob at, say, 100.
* It passes, `FlGSyncWt` > 0, `FlGSyncTmo` = 0: the ordering failure is a CPU-vs-GPU race (the release
  outran the releaser's work), finding 1 (the acquirer's work is not ordered by the packet; the CPU wait
  covers for it).
* It still fails with `FlGSyncWt` > 0 and no timeouts: the releaser's work was complete before the release;
  the failure is not the gate's ordering at all (look at the acquirer, finding 1 / 5).
* `FlGSyncTmo` > 0: the point did not retire within the bound; read `FlGEv*` / `FlGReb` before concluding.

### Not done / not verified

* Nothing was built as a driver (no WDK target here) or run in the guest. The pure code has host tests;
  the `kmd_render` code was type-checked against a stub harness that mirrors the module visibility and the
  signatures used (not the WDK bindings), and parsed with `rustfmt`.
* Whether the SubmitCommand-side `hContext` read is valid on the virtual path when
  `DmaBufferUmdPrivateDataSize` is nonzero is not assumed: the trace is skipped there, exactly like the
  execution-record decode (`FlGSub` would then under-count; `FlGVerdict` 1 on that path means "not traced").
* Read after the next run: `FlGSub` vs `FlGRec`, then `FlGMis` / `FlGEmpty`, `FlGImm`, `FlGLagAvg`, `FlGUnord`,
  `FlGVerdict`, and the knob experiment above.
