# RM fence markers (S4): a present that retires on an RM semaphore value

Status: ABI specified here, in `protocol/src/rm_fence.rs` (Rust),
`protocol/include/helios_rm_fence.h` (C, WDDM records) and
`guest/rmclient/src/helios_nvrm_escape.h` (C, NVRM ops). Plan:
`research/dxvk-on-nvk` `guest/windows/docs/dxvk-on-nvk.md` section 3.5 level 2, stage S4.
Nothing here is built or run on Windows; the pure rules are host-tested in `kmd_logic`.

## What it is for

Today a present names its completion boundary either as nothing (legacy: wait for the
transport's current wire fence), as a Venus stream point `(ctx, value, cookie)`, or as
"already complete" (`value == 0`, CPU-complete: the UMD waited on the CPU for its own GPU
work first). An NVK-on-RM producer has no Venus stream; its work is an RM semaphore value
(`SEM_EXECUTE` release). The host already turns such a value into a one-shot event:
nvidia-drm `SEMSURF_FENCE_CTX_CREATE` (0x54) and `SEMSURF_FENCE_CREATE` (0x55) return a
backend handle that sends `EventReady` when the semaphore reaches the value
(`docs/SYNC.md`, gated on config feature bit 11). The KMD already records the handle a
forwarded 0x55 returns as a fence (`device_type` 511, `kmd_logic::nvrm_fence`,
`virtio/nvrm.rs`) and already keeps the early-fire latch (`FenceBook`,
`ready_latched`). This document adds the consumers: a present may name that fence as its
completion boundary, and the KMD retires it from the `nvrm_events` DPC.

Three carriers, one fence object:

| | carrier | what retires on the fence |
|---|---|---|
| (a) main | `HELIOS_NVRM_OP_SCANOUT_PRESENT` with flag `RM_FENCE`, `rm_fence_handle` at offset 52 | the host `ScanoutFlip` is sent when the fence fires |
| (b) | `HERF` (`HeliosPresentRefreshCmdFence`, 48 B) and `HEPR` (`HeliosPresentRenderCmdFence`, 96 B) | the present's DMA fence, scanout bind and windowed blit |
| (b) | `HE12` version 4 (`HeliosD3D12SubmitCmdV4`, 48 B) | the ExecuteCommandLists batch's DMA completion (the runtime's monitored-fence signals) |
| (b) | `HEFL` (`HeliosFlushGateCmd`, 48 B, `flags = RM_FENCE`) | a D3D11 `pfnFlush` packet's DMA fence, so a keyed-mutex release waits for the real work (`flush-gate.md`) |

## The fence object (all carriers)

* **What:** a backend handle from a forwarded `SEMSURF_FENCE_CREATE` (ioctl low 16 bits
  `0x6455`) on a DRM-node handle of the caller, recorded by the KMD under
  `DEVICE_TYPE_FENCE` (511) at the reply. Create it right before the present, in the
  presenting process, through NVK's NVRM device handle. 0 means "none".
* **Ownership passes to the KMD when the carrier is accepted** (status `OK` for (a); the
  `DxgkDdiRender` that parses the record for (b): one attach per present, at Render, and the
  Present that follows only carries the resulting boundary). From then on the UMD/NVK must NEVER `Close`,
  `EVENT_REGISTER`, `FORWARD` on or reuse the handle: the KMD re-tags the entry to its own
  owner (`DeviceOwner::KMD_RM`), so every such call answers `NOT_OWNED`. The KMD closes the
  handle on the host when it fires, and on every teardown (see below).
  A REFUSED carrier whose call status the caller sees ((a), and `HE12` v4: `Render`
  fails) changes nothing: the handle stays the caller's, who may CPU-wait on the
  semaphore and `Close` it as usual. A `HERF` / `HEPR` tail is the exception, because
  its `Render` returns success whatever became of the tail and the UMD cannot learn a
  refusal: **for ANY parsed fence tail whose handle is a fence of the presenting
  process, the KMD takes and closes the handle, attached as the present's marker or
  not** (see (b) below). A handle that is not such a fence (another process's, not a
  fence, already the KMD's) is never touched.
  When the KMD takes a handle, what its creator had registered on it (`EVENT_REGISTER`)
  is released at the same time: the KMD's own close does not release registrations
  the way a user `Close` does.
* **The wait value is baked into the fence.** `rm_fence_value` exists only in the (b) tail
  and is diagnostic: it is never read for a decision. (a) has no room for it and drops it.
* **Fire:** the host sends one `EventReady{handle}` whose header `status` is 0 or the
  fence's error (`-ETIMEDOUT` after nvidia-drm's at most 5 s, etc.). A fire with any
  status retires the present exactly like success; an error status is only COUNTED
  (`FsFErr` / `RmGErr`). The semantics are therefore "the fence is no longer pending",
  not "the work succeeded": a timeout means the GPU work is hung, and showing/completing
  the frame is the same lie the `WddmHeadMs` rebase already tells (bounded by the 5 s
  host timeout; no KMD timer exists).
* **Early fire** (the semaphore was already past the value, so the fence fired before the
  present arrived or before the create's reply was recorded) is the common case. The
  existing paths cover both halves: `FenceBook` hands a fire that raced the create reply
  to `commit_nvrm_fence` (`ready_latched`), and a fire after the handle is recorded sets
  the same latch. Attaching consumes it and treats the present as already complete: for
  (a) the flip is sent before the escape returns; for (b) the point is born fired.

## (a) `SCANOUT_PRESENT` with a fence

```c
HeliosNvrmScanoutPresent {            // 64 bytes, unchanged size
  HeliosNvrmHeader head;
  uint32_t handle;                    // the DRM-node handle given to SCANOUT_SET
  uint32_t gem;
  uint32_t flags;                     // bit 0 HELIOS_NVRM_SCANOUT_PRESENT_FLAG_RM_FENCE
  uint32_t rm_fence_handle;           // @52, was `reserved`; 0 unless the flag is set
  uint64_t out_seq;
};
```

* No flag: today's behaviour (the flip is sent synchronously). This is also what
  NVK sends when no fence is wanted, i.e. after it waited on the CPU (value 0 /
  CPU-complete). One exception, to keep the order: while fenced presents wait (or are
  being sent) a plain `PRESENT` queues behind them as an already-ready entry and returns
  at once (it can then answer `QUEUE_FULL`, and a failed send is counted, not returned).
* With the flag the KMD validates, in this order (first failure wins; nothing changes on
  a refusal):
  1. `rm_fence_handle != 0` and `gem != 0` (`BAD_RANGE`); the capability exists
     (`UNSUPPORTED`).
  2. `handle` is the caller's live source (`NOT_OWNED` / `FORBIDDEN` / `NO_SOURCE` as for
     a plain `PRESENT`).
  3. `rm_fence_handle` is a handle of the SAME NVRM owner as the source (`NOT_OWNED`: not
     the caller's or unknown, and one answer so nothing is learnt of others' handles),
     whose kind is a fence (`FORBIDDEN` otherwise). An already attached handle is the KMD's,
     so it answers `NOT_OWNED` like any handle that is not yours;
     `HELIOS_NVRM_ST_FENCE_ATTACHED` (15) is reserved for a future distinction and is not
     returned today.
  4. fewer than `HELIOS_NVRM_SCANOUT_FENCE_DEPTH` (8) presents wait (`HELIOS_NVRM_ST_QUEUE_FULL`
     = 16; present without a fence, or retry).
* On `OK` the KMD minted `out_seq` (strictly increasing, as for every flip), queued
  `{generation, gem, seq, fence}` and RETURNS AT ONCE. The flip is sent from the HPD
  worker (PASSIVE; the DPC cannot do the host round trip) when the fence fires, or before
  the escape returns when the fence had already fired and nothing waits ahead of it.
* The source's lapse timer is pushed out at `PRESENT` as for a plain one and again when a
  flip is sent.
* **Ordering: FIFO per source, with ready-prefix coalescing.**
  * Flips are sent in submission order. A later present whose fence fires first WAITS
    behind the earlier one (the display never shows frame N+1 while frame N's fence is
    pending; with one NVK timeline the fences fire in order anyway).
  * When the worker finds the head ready, every immediately following READY entry
    supersedes it: only the newest consecutive ready frame is sent, the skipped ones are
    counted (`FsFSkip`) and their fences closed. This is "latest frame wins" exactly when
    the display is behind, and "every frame" when it keeps up. A not-yet-fired entry is
    never dropped or replaced: replacing it would free an image whose GPU work may still
    be running, and the client cannot tell.
  * Why not replace-the-queued-one: the host flip has no completion or release signal.
    The only safe reuse rule for the client's N rotating images is "do not write an image
    until a LATER frame has been shown". Dropping a still-pending frame would make that
    rule unsound for the image behind it.
* **What the client must assume about image reuse (v1):** with N >= 3 images, do not
  render into the image of present P until the fence of present P+1 has fired (CPU-read
  the semaphore) AND P+1's call returned. The flip of P+1 follows its fence by one worker
  wake (sub-millisecond when idle), so there is a window of that length in which P is still
  on the host. A precise signal would need a `sent_seq` readable by the client (open
  question below). `PRESENT` returning does NOT mean the flip was sent any more.
* **The source ends** (RELEASE, close of the DRM file, device destroy, process exit, lapse,
  transport reset): queued entries of that source are dropped unsent (`FsFDrop`), their
  fences are closed. The worker notices on its next wake (every end path wakes it). A flip the host refuses is counted (`FsErr`, as today) and the frame
  is lost silently: the client has no completion channel.
* `QUERY_CAPS.supported_ops` bit 32 (`HELIOS_NVRM_CAP_SCANOUT_FENCE`) says the flag works
  (see "Capability bits").

## (b) WDDM carriers

All three go through `pfnRenderCb`, are parsed by `DxgkDdiRender`, and are attached at the
point the existing stream marker is resolved (`DxgkDdiPresent` for `HERF`/`HEPR`,
`DxgkDdiRender` for `HE12`). Common tail (16 bytes):

```c
struct HeliosRmFenceTail {
  uint32_t rm_fence_handle;   // 0 = none
  uint32_t flags;             // 1 FENCE (handle valid), 2 COMPLETE (HE12 only, no handle)
  uint64_t rm_fence_value;    // diagnostic only
};
```

* **`HERF`** (`HeliosPresentRefreshCmd`, 32 B): append the tail, 48 B. No version bump: an
  older KMD ignores the extra bytes (the present then follows the legacy wait), whereas a
  bumped version would be ignored whole and lose the scanout-refresh arm. The tail is read
  only when `CommandLength >= 48`. The stream tail (`present_ctx_id`, `present_value`,
  `present_cookie`) must be zero when a fence is carried: the two markers are EXCLUSIVE; a
  record carrying both has its stream marker honoured and its fence NOT attached
  (counted `RmGRef`); the fence handle is still taken and closed (`RmGTake`, see the
  refusal rule below).
* **`HEPR`** (`HeliosPresentRenderCmd`, 80 B): append the tail, 96 B, set
  `present.reserved |= HELIOS_PRESENT_PRIVATE_FLAG_RM_FENCE` (bit 3). Honoured only when
  the command covers all 96 bytes and the flag is set; same exclusivity. Same reasoning
  for not bumping the version (the file's own convention: appended tails are guarded by
  flags and coverage).
* **`HE12`** (`HeliosD3D12SubmitCmd`): version 4, 48 B = the v3 record + the tail.
  FENCE variant: `ctx_id = value = cookie = gpu_wire_fence = 0`, `flags = FENCE`, nonzero
  handle. CPU-complete variant: the same zeros, `flags = COMPLETE`, handle 0: the record
  is accepted and carries no boundary (the producer waited on the CPU), so the DMA packet
  retires by the ordinary wire rule. The stream variant is the unchanged v3. **Here the
  version IS bumped**: an older KMD refuses a v4 record (`Render` fails with
  `STATUS_INVALID_PARAMETER`), which is the right failure for D3D12, where silently
  skipping the completion proof would let a runtime fence signal early. Hence the gate on
  `HELIOS_NVRM_CAP_PRESENT_FENCE`. `HE12` is carrier (a)-like: the UMD sees `Render`'s
  status, so every refusal leaves the handle the caller's, and everything that can
  still refuse is checked BEFORE the fence is taken: a context's last bound stream must
  be this process's gate (or none yet; a context that mixed stream and fence `HE12`s is
  refused up front, not after the take), and `Render`'s own argument checks (patch
  lists) come before any parsing of tails.
* **Refusal rule for `HERF` / `HEPR` (the UMD cannot see it).** Their `Render` returns
  success whatever became of the tail, so a handle the KMD merely declined would leak
  one of the process's 128. For ANY parsed tail (`flags != 0` or a handle, and
  `CommandLength` covering it) whose handle is a fence of the presenting process (the
  ownership check below), the KMD takes and closes the handle whether or not the
  marker is attached: both markers in one record, a partial stream tail, a tail
  without `FENCE`, no gate or point room, an unserved host. `RmGRef` counts the markers
  refused, `RmGTake` the handles taken and closed without a marker. Nothing is taken
  that is not such a fence.
* **Ownership check.** The Render/Present context belongs to the D3DKMT device of the
  runtime, NOT to the NVRM escape device that created the fence (NVK's NVRM device
  differs from the WDDM device), so "same device" cannot be verified at `DISPATCH` and is
  not required. The check is the documented edge the existing stream markers use: the
  context's `hKmdProcess` (`DeviceContext::creator_process`) must equal the process recorded
  when the fence was created. The KMD records that process in the fence's table entry at
  the 0x55 reply (the escape device is alive then; reading it later, from Render, could
  race its destruction). The entry must also be a fence (`device_type` 511), not already
  attached, and not owned by the KMD already. Any failure drops the marker (counted
  `RmGRef`) and the present follows the legacy rule; for `HE12` nothing is closed or
  consumed, for `HERF` / `HEPR` the handle is taken and closed when it passes the same
  claim (see the refusal rule above). The check is read-only and runs BEFORE a gate
  or stream slot is taken, so a junk handle opens no gate (a gate would last until the
  process exits).
  Cost: a linear scan of the (at most 1024) handle table under `virtio_lock`, no
  allocation, no PASSIVE-only lock; the table work runs at `DISPATCH` inside
  `DxgkDdiRender` (PASSIVE), which then releases what the creator registered on the
  handle and, with no HPD worker, runs the worker's pass (below).
* **Representation (KMD-internal; the ABI above does not depend on it).** The boundary
  stays in the existing tagged stream namespace (bit 63 set, generation-qualified 31-bit
  handle, 32-bit value), so every consumer of boundaries keeps working unchanged:
  `merge_stream_boundary`, the windowed-blit and fast-bind staging, `execution_completion`
  waits, dead-stream discharge. The handle names a per-process "RM gate", a slot of the
  present-stream table with no Venus context (`ctx_id = 0`, `cookie = 0`, so no marker or
  tag can name it) whose "retired value" is advanced by fences instead of used-ring
  responses. Each attached fence takes the gate's next point `p = 1, 2, ...`; the
  boundary is `encode(gate_handle, p)`. A point is ready when it and every earlier point
  of the gate fired (prefix retirement, so a merged DMA buffer carrying two presents of
  one process waits for both, and an out-of-order fire never lets a later point read as
  retired before an earlier one).
* **Two handles in one DMA buffer.** The private record carries ONE boundary, and a
  process can now put two handles in a buffer (a gate and a registered stream: a fenced
  present beside the CPU-complete marker `(ctx, 0, cookie)`, or a stream beside a
  fence). Merging never fails the Present (its fence was already taken at Render)
  (`kmd_logic::rm_fence_present::merge_stream_boundaries`): the same handle takes the
  larger value; a value-0 ("already complete") boundary of either handle is no
  dependency and gives way, lossless; two handles that both wait keep the OLDER wait
  and drop the later present's, counted `PrBndDrop` (a present may then be released
  before its producer finished). Why the older and not the wire watermark: that
  fallback would drop BOTH waits, and the legacy watermark does not cover an RM gate's
  work at all. A WindowedBlt request (token) is valid only under its own boundary, so
  it replaces a plain boundary of another handle (`PrBndDrop` if that one waited), and
  two requests of different handles in one buffer are the one case still refused (the
  request is cancelled, as before).
* **Retirement.** `EventReady{handle, status}` in the `nvrm_events` DPC marks the fence
  fired (once; a second `EventReady` for the number is ignored), fires its gate point,
  advances the gate's retired value, observes pending execution waits, and the same DPC
  pass re-evaluates the WDDM FIFO head, the deferred fast bind and the windowed blit, all
  of which read `scanout_boundary_ready`. The handle is queued for a host `Close` that the
  HPD worker sends (PASSIVE).
* **Limits.** 8 gates (processes), 128 unretired points per gate (a D3D12 app with more
  `ExecuteCommandLists` in flight than that has its next v4 record refused: the UMD then
  falls back to a CPU wait plus the `COMPLETE` variant), and a gate refuses past 2^32 - 1025
  points (about 49 days at 1000 presents/s; it is recycled when its process exits). The
  KMD holds at most 512 fence handles as its own at a time (attached or owed a close,
  `MAX_NVRM_ATTACHED_FENCES`): a fence leaves its creator's 128-per-process quota when
  the KMD takes it and is counted there instead, so attached fences can neither fill the
  table nor starve the KMD's own RM client (which shares the `KMD_RM` owner); past the
  bound the carrier is refused like a full gate (`NoRoom`, `QUEUE_FULL` for (a)). The
  generic `WddmHeadMs` bound (250 ms by default) applies to a gate boundary like to any
  stream boundary: a frame whose fence takes longer is released early, counted `WfBReb`.
  It does not apply to an `HE12` (execution) packet, which keeps every completion
  obligation and is only ended by its fence, a purge of its gate, or a scheduler reset.

## What the UMD/NVK sends when it wants no fence

* (a): `flags = 0`, `rm_fence_handle = 0`. The flip is sent before the call returns.
* (b) `HERF`/`HEPR`: the existing "already complete" marker `(ctx, value = 0, cookie)` on a
  registered stream, or no tail at all (the legacy rule: the DMA fence waits for the
  transport's wire fence at submission, a cheap condition on a system with little Venus
  traffic). Nothing new is needed.
* (b) `HE12`: the v4 record with `flags = COMPLETE`.

## Teardown and reset

A pending fence of an attached present is released by every path that ends the present:

| event | the present | the fence handle |
|---|---|---|
| fires (any status) | retires | closed by the HPD worker |
| (a) source released / DRM file closed / device destroyed / process exit / lapse | queued flips dropped unsent | closed (queued for the worker) |
| (b) device destroyed / process exit | the process's gate is purged like a stream whose owner died: waits that named it are discharged and counted (`RmGCan`), so no DMA fence stays pinned behind a dead process | closed |
| transport lost or reset (`StopDevice`, TDR reset, failure) | gates and queued flips are purged exactly like Venus streams (`purge_all_present_streams`): the present's wait is DISCHARGED, never treated as satisfied, and counted | the sweep closes every handle on the host (`close_all_on_host`) |

The retire-on-teardown for (b) follows the existing dead-stream rule: a purged stream is
an explicit cancellation, the DMA fence completes (otherwise the adapter-wide, head of line
WDDM FIFO would stall until the `WddmHeadMs` rebase), and the event is counted.

Two kinds of wait name a gate, and a purge ends both explicitly. A plain present or
windowed-blit wait (`stream_boundary` of a FIFO entry) is discharged by
`discharge_dead_present_stream_waits`, as for any dead stream. An `HE12` EXECUTION wait is
NOT touched by that function (it skips every entry with an execution wait, and an
execution packet is never rebased: it must keep every completion obligation), so the gate's
own cancel (`rm_gate_cancel`, which runs for the process purge and for the transport
purge alike) latches the execution waits that named its stream handle as ended, in the
same step that hands its fences to the closer. Without that, a purge would leave such a
packet pinned at the head of the adapter-wide FIFO for good (nobody can fire a closed
fence). A Venus stream's execution waits keep their own, unchanged rule. In practice
dxgkrnl has retired a process's DMA buffers before `DestroyProcess`; this is the net
under an abnormal exit.

## Capability bits

`HeliosNvrmQueryCaps` has no spare field and keeps its 88 bytes. `supported_ops` is a
64-bit mask indexed by op number, and op numbers are `u32` and small, so bits 32..63 are
free for capabilities:

| bit | | set when |
|---|---|---|
| 32 | `HELIOS_NVRM_CAP_SCANOUT_FENCE` | the event queue is up, the host advertises `NVGPU_CFG_DRM_FENCES` (features bit 11) and the scanout ops exist |
| 33 | `HELIOS_NVRM_CAP_PRESENT_FENCE` | the same, and the WDDM carriers are compiled in |

## Statuses

`HELIOS_NVRM_ST_FENCE_ATTACHED` = 15, `HELIOS_NVRM_ST_QUEUE_FULL` = 16 (13 and 14 are the
foreign scanout's `SCANOUT_BUSY` / `NO_SOURCE`). Reused: `NOT_OWNED`, `FORBIDDEN`,
`BAD_RANGE`, `UNSUPPORTED`, `NO_SOURCE`.

## Counters (names are at most 14 bytes; published by `publish_nvrm_counters`)

(a): `FsFQue` entries queued, `FsFSent` flips sent from the queue, `FsFFire` fences fired,
`FsFErr` fired with an error status, `FsFEarly` queued already fired, `FsFSkip` ready entries
superseded, `FsFDrop` dropped unsent, `FsFRef` refused, `FsFFull` refused for a full queue.
Waiting now = `FsFQue - FsFSent - FsFSkip - FsFDrop`.
(b): `RmGAtt` points attached, `RmGFire`, `RmGErr` (including a fence that fired with an
error before its create was recorded), `RmGEarly`, `RmGCan` cancelled by
teardown, `RmGRef` markers refused (not owned / not a fence / already attached / both
markers / no gate room or fence quota), `RmGTake` handles of a `HERF` / `HEPR` tail
taken and closed although no marker was attached (the refusal rule), `PrBndDrop`
boundaries of two handles that met in one DMA buffer and cost the later present its wait.
Common: `NvFenceCl` (existing) counts every fence handle closed, including the KMD's;
`FnCloseErr` counts the KMD's closes the host did not take.

## Locks and IRQL

* Attach (a) runs in the PASSIVE escape under `virtio_lock` then the leaf `FENCES` lock;
  attach (b) in `DxgkDdiRender` under `virtio_lock` (`DISPATCH`): table scans and
  fixed-array writes only, no allocation, no wait, no PASSIVE-only call. Lock order:
  `wddm_notify` -> `virtio_lock` -> `FENCES`; `STATE` (foreign scanout) is never taken under
  either. Gate and queue storage is reserved at transport init.
* Fire runs in `drain_nvrm_events` under `virtio_lock` in the DPC: it sets flags, advances
  the gate, observes waits, and returns "work for the worker"; the caller then
  `signal_hpd`s (`KeSetEvent`, `Wait = FALSE`). The host `Close` and the flip send are the
  HPD worker's, outside every lock.
* Double retire is impossible by construction: a point and a queue entry have one
  `fired` bit set by the first `EventReady`; the handle's `Close` is taken from the table
  by the one worker (take-then-send). A `Close` is only ever owed for an entry that is
  the KMD's (owner `KMD_RM`) and attached to something, so a stale queue entry or a
  purged gate's point cannot close a live fence of someone else if the host reused the
  number.
* **No HPD worker** (render-only `DisplayHalf=0`, or its creation failed). The fence path
  does not need one (`rm_fence_served` checks the event queue and the host feature, not
  the worker), so the work that is the worker's goes to a PASSIVE thread that is there
  anyway: every NVRM escape ends with the fence pass (as it already ends with the
  counter mirror), and the `Render` that takes a fence runs it too. Making the fence
  path require the worker instead would have switched off the `HE12` v4 path, the one
  render-only use. With a worker, none of this runs.
* **`DestroyProcess`** runs for every process exit, so it first reads one atomic (the
  count of open gates) and returns at once when it is zero: no `wddm_notify_lock`, no
  `virtio_lock`, and the adapter pointer is not even dereferenced. A gate lives in the
  transport, which the adapter owns, and the transport's `Drop` purges every gate (and
  zeroes the count) before the adapter can go, so while the count is nonzero the
  adapter is alive. The purge it then runs only wakes the worker for the `Close`s it
  owes; with no worker the next NVRM escape or fence `Render` sends them.
* **The pass is bounded** (the HPD worker runs it, and `stop_hpd` joins the worker for
  5 s, twice): each host round trip (a queued flip, a `Close`) waits at most 2.5 s, and
  the pass ends at the first timeout (the host is not answering) and once `hpd_stop` is
  set; what it did not reach stays queued or owed (the debt flag is raised again) for
  the next pass or the transport sweep. One pump runs at a time: a caller that finds it
  busy leaves a note and retries the acquire once after the note, so the holder's exit
  cannot strand it (the note is never left set with nobody pumping); there is no
  `signal_hpd` there, since a failing caller that is the worker itself would spin.

## Deviations from the requesting side's list

1. `rm_fence_value` is not in (a) (no room, diagnostic only); it is in the (b) tail.
2. (b) `HERF`/`HEPR` are not version-bumped; `HE12` is (reasons above).
3. The capability bits live in `supported_ops` bits 32..63, not a new field.
4. (b) ownership is "a fence recorded in the same PROCESS", checked against a process
   stored with the handle, because the presenting device is not the NVRM device.
5. KMD-closes-on-fire applies to (b) as well, with the same "never Close afterwards".
6. (a) is FIFO with ready-prefix coalescing (not replace-the-queued-one): see above.

## Open questions for the UMD/NVK side

* Do you need a precise "this flip was sent" signal (a `sent_seq` the client can read,
  e.g. a field of a new `SCANOUT_STATUS` op, or an event)? Without it the image-reuse rule
  above has a worker-wake-sized window.
* NVK D3D12 (`HE12`) has no Venus stream: confirm the v4 FENCE/COMPLETE variants cover
  `ExecuteCommandLists` (one fence per batch, created right before it).
* One fence per present means one `0x55` round trip (about one escape) per frame on the
  presenting thread. If that is too slow, a batch create is a host change.
