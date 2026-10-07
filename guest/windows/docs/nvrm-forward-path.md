# HELIOS_ESCAPE_NVRM FORWARD: per-call cost and the fast path

Scope: `D3DKMTEscape` -> `ddi/escape.rs::escape_nvrm` -> `virtio/nvrm.rs::forward`
-> `virtio/ctrl.rs::raw_roundtrip` -> control virtqueue 0 -> host
(`host/backend/device`, a vhost-user process) -> used ring -> interrupt -> DPC ->
waiter. Everything below was found by reading; nothing was measured. Figures
marked "est." are order-of-magnitude estimates for a KVM guest, to be replaced by
the counters named at the end.

What the host contributes (read only): the backend is a vhost-user process.
A kick is a KVM ioeventfd, the backend thread wakes from epoll, serves the
message (an ioctl into the host NVIDIA driver), and `signal_used_queue()` raises
the guest interrupt through an irqfd. The guest sees a reply no sooner than the
thread wake plus the host ioctl, so tens of microseconds (est. 25-60 us for a
cheap RM call); anything that allocates or maps is milliseconds.

## 1. Cost inventory per `Ioctl` forward, BEFORE (steady state)

Caller thread, in order:

| # | Step | Cost kind |
|---|------|-----------|
| 1 | `dxgkddi_escape`: header copy, owner token, flag counters | cheap |
| 2 | `escape_nvrm_op`: `with_virtio(nvrm_epoch)` | lock hold #1 |
| 3 | `forward`: msg checks; `owned()` | lock hold #2 |
| 4 | `check_ioctl` | cheap |
| 5 | `raw_roundtrip`: `Vec::try_reserve_exact(resp_cap)` **and `resize(resp_cap, 0)`** | pool alloc + free, and a memset of the caller's whole reply capacity (up to 1 MiB) |
| 6 | `reap_parked`: `begin_parked_reap` (nonempty, because the previous forward parked its entry) + `recycle_dma_buffers` + `finish_parked_reap` | lock holds #3, #4, #5, and three `Vec` swaps |
| 7 | `take_dma_buffer(total)`: linear scan of the DMA pool (up to 256 entries, `min_by_key`) under the spinlock | lock hold #6; on a miss `MmAllocateContiguousMemory` + zero-fill |
| 8 | memcpy request -> DMA buffer | necessary |
| 9 | `SyncWaitBlock` `KeInitializeEvent` | cheap |
| 10 | enqueue: `drain_used`, `enqueue_core` (4 descriptors, 4 x `MmGetPhysicalAddress`), `inflight.push`, `should_notify`, doorbell MMIO write | lock hold #7; the doorbell is a VM exit (est. 1.5-3 us) |
| 11 | `wait_block`: `KeWaitForSingleObject` with a 1 ms slice (timer arm and cancel), block, wake (context switch; on an idle vCPU a halt exit and a wake of the halted vCPU) | est. 5-30 us on top of the host latency |
| 12 | copy reply `Vec` -> `resp_out`, free the `Vec` | memcpy + free |
| 13 | `escape_nvrm` tail: `CALLS.fetch_add` and `LAST_SHAPE.swap` | two LOCKED instructions on lines every forwarding thread shares |
| 14 | every 256th forward, and on every open/close/map/pin/unpin/event change: `publish_nvrm_counters` | ~40 synchronous registry writes (measured ~1 ms), run INSIDE that call: the 2.1 ms Open+Close and the 1-2 ms p99 spikes; a pin/unpin storm made it per-call |

Completion side (interrupt CPU), after the device writes the used ring:

* ISR at DIRQL: on INTx (`MSISupported=0`, or the fallback), a read of the virtio
  ISR-status register (MMIO, a VM exit) and the level-triggered INTx EOI; with
  `MSISupported=1` (the opt-in, message mode, `docs/msi-interrupts.md`) neither: the ISR
  counts the vector and queues the DPC.
* DPC: `DxgkCbNotifyDpc`, then `drain_used_and_complete`: the FIRST `with_virtio`
  drains and wakes the waiter (copies the reply into the waiter's `Vec` at
  DISPATCH under the lock, `KeSetEvent`, parks the entry); then about seven more
  lock holds (deferred fast bind, fast-failure wake, worker bind, completed
  bind, the notify-lock block, WDDM completion loop) that find nothing to do for
  NVRM-only traffic but contend with the waiter's next lock hold.

Lock holds on `virtio_lock` per forward on the caller's thread: **7**.
Allocations per forward: **1 heap alloc + free + memset**, plus the pool
bookkeeping. Locked read-modify-writes on shared lines in the escape tail: **2**.

Notification suppression: `VirtQueue::new(.., event_idx = false)`. The host
offers `VIRTIO_RING_F_EVENT_IDX` (`conduit-backend.rs::features`), the guest
negotiates only `CONDUIT_REQUIRED_FEATURES`. With the legacy flag, `should_notify`
is true whenever the device has not set `VRING_USED_F_NO_NOTIFY`, which a
vhost-user backend that drains in a loop sets only while it runs. For ONE
thread issuing synchronous calls the doorbell is needed every time (the device
idles between calls), so EVENT_IDX would not help that case; it helps
concurrent forwards.

## 2. What this change does

All of it is on the forward path only; the Sync control commands and Venus
paths are untouched except where noted.

1. **The counters mirror is off the escape path** (`escape.rs`
   `nvrm_publish_counters_if_due` / `nvrm_publish_service`, `hpd.rs`,
   `lifecycle.rs`, `kmd_logic::nvrm_fastpath::publish_gate`). The escape computes
   the same trigger as before (session shape moved, or a new 256-forward bucket)
   from plain loads; on a trigger it does one atomic `swap` and, only if no
   request was outstanding, one `KeSetEvent` on the HPD worker's event. The ~40
   registry writes run on the HPD worker (a PASSIVE system thread alive for the
   whole device lifetime), at most once per 250 ms. While a request is
   outstanding but not yet due the worker's wait is bounded to 250 ms, so the
   trailing state of a burst is published even if no escape follows (idle
   correct), and `DxgkDdiStopDevice` publishes once more after joining the
   worker. The present edge still publishes as before. The call-count trigger
   reads the existing `NvIoctl + NvOther` counters instead of a private
   `fetch_add`, and the memo is written only by the worker: no locked RMW on the
   hot path. If the worker could not be created (`StHpd`), the mirror falls back
   to the present edge and StopDevice.
2. **Reply buffer without heap or memset** (`ctrl.rs::raw_roundtrip`). A reply
   capacity up to 1 KiB lands in an uninitialised buffer on the caller's stack;
   larger replies use a heap `Vec` reserved fallibly and also not zero-filled.
   Only the `used` bytes the drain wrote are read back, through a raw copy.
   Soundness is the `SyncWaitBlock`'s own: the drain writes `dest` only under
   the lock and only while `waiter` is set, `abandon_sync` clears `waiter` under
   that lock, and `dest` is declared before the wait block so it outlives it.
3. **Completed forward buffers go straight back to the DMA pool**
   (`gpu/mod.rs`, Raw arm of `drain_used`). The reply was already copied out, so
   nothing reads `meta` again. A push into the reserved pool neither allocates
   nor frees, which is what the fast-bind command buffer already relies on. A
   buffer the pool refuses (too big, pool full) parks as before and is freed by
   the PASSIVE reap. This removes the begin/recycle/finish reap from the next
   forward.
4. **One lock hold for the reap check and the pool take**
   (`ctrl.rs::reap_parked_work`, `raw_roundtrip`).
5. **One lock hold for ownership and device generation** (`nvrm.rs::forward`,
   `escape.rs::nvrm_forward`). The epoch is sampled in the same hold as the
   ownership check, i.e. still before the message goes out; non-Ioctl messages
   read it as before. Refusals that never take a lock read it on the way out.
6. **Pool take stops at the first page-sized buffer** (`gpu/mod.rs::take_dma_buffer`,
   `DmaBuffer::MIN_CAPACITY`): same buffer chosen (smallest that fits, first
   among equals), without scanning all 256 entries under the spinlock.
7. **Bounded pre-wait spin** (`ctrl.rs::spin_for_completion`,
   `kmd_logic::nvrm_fastpath::spin`). After the doorbell, PASSIVE only, no lock
   held, the caller polls the wait block's `done` for up to `NvSpinUs`
   microseconds (default 50, 0 = off, clamped to 200), 8 polls per clock read.
   It is a HINT: the caller then enters `wait_block` regardless, and
   `KeWaitForSingleObject` stays the only completion-side exit (a lock-free
   `done` read must not authorise leaving; see `wait_block`). A reply seen in
   time makes the wait return at once: no timer, no context switch, no halted
   vCPU to wake. It never touches the transport or its lock, so it cannot slow
   the DPC. Adaptive: a spin that hit earns 2 credit, a miss costs 4 (credit
   grows only while more than 2/3 of spins hit); with no credit one call in 64
   probes. A host that answers in milliseconds therefore costs about one budget
   per 64 forwards.

   Deliberately NOT "spin on `drain_used` with the interrupt suppressed": a
   waiter draining the ring takes `virtio_lock` on every poll (and fights the DPC
   and other callers for it), and suppressing the used-ring interrupt is a
   queue-wide switch, so Venus completions sharing queue 0 would be delayed
   until the spinner stops. Polling the waiter's own flag leaves the interrupt
   and the DPC exactly as they are.
8. **The woken thread gets a priority increment** (`gpu/mod.rs`, Raw arm):
   `KeSetEvent(.., IO_VIDEO_INCREMENT, ..)` instead of `IO_NO_INCREMENT`, for
   raw forwards only.

Lock holds on the caller's thread: 7 -> **3** (epoch+owned, reap+take, enqueue).

## 3. Expected savings (est.), against the host session's v307 numbers

Measured by the host session (v307, p50, win11 vs host native): QUERY_CAPS
alone 0.7 us; one RM control 58 us (1.4 native, backend share 5.5 us); event
round trip 142 us; Open+Close 2134 us; GPU map+unmap 2 MiB 334 us.

| Change | Saving |
|--------|--------|
| 1. mirror off the escape path | the whole ~1 ms registry mirror from every Open, Close, Map, Pin, Unpin and EVENT_REGISTER and from every 256th forward: the mirror is most of the 2134 us Open+Close (expect the low hundreds of us; the rest is host work), and the 1-2 ms p99 spikes on plain controls should vanish. Two locked RMWs per forward (est. 0.05-0.2 us). |
| 7. pre-wait spin | est. 15-20 us per forward when the reply lands inside 50 us (the 58 us control: the reply is ~5.5 us backend + wake chain); 0 when it misses, plus at most one budget of CPU per 64 forwards on a slow host. The least certain number; A/B with `NvSpinUs=0`. |
| 2. stack reply, no memset | one pool alloc+free (est. 0.2-0.5 us) and a memset of the reply capacity (a 64 KiB capacity costs est. 2-3 us) |
| 3. pool return from the drain | three lock holds and three `Vec` swaps per forward (est. 0.3 us), fewer `MmAllocateContiguousMemory` misses under concurrency |
| 4, 5. merged lock holds | one lock hold each (est. 0.1 us each): 7 -> 3 on the caller's thread |
| 6. pool scan early exit | est. 0.1-0.4 us under the lock with a full pool |
| 8. wake increment | est. 0-10 us, only when guest CPUs are busy; nothing when the spin hits |

Items 2-6 and 8 are together a few microseconds (the coordinator's "smaller
items" 5-10 us, of which the DPC lock consolidation below is NOT included).

## 4. Safety properties kept

* No allocation, and no PASSIVE-only drop, under the spinlock or at DISPATCH:
  the drain's pool push is inside capacity reserved at construction and checked
  (`dma_pool_accepts`); a refused buffer is parked, never dropped there. The
  heap reply `Vec` is created and dropped at PASSIVE in the caller.
* The KMD still reads only `MsgHeader` / `IoctlReq` fields it validated; the
  reply is never interpreted beyond what it did before.
* Pin splice (`forward_pinned`) is byte-for-byte unchanged, including its
  `Vec`; it runs once per registered buffer, not per call.
* `abandon_sync` / timeout semantics unchanged: a timed-out forward leaves
  `waiter = None`, the late completion copies and signals nothing, and its
  buffer now goes to the pool (device finished with it) instead of the park list.
* The `0xA` invariant of `wait_block` holds: nothing exits on `done`.

## 5. Evidence to collect (the host session)

* `NvSpinHit` / `NvSpinMis` (registry, with the other `Nv*`): hit rate of the
  spin. Both flat at 0 with forwards running means it is off.
* Existing `DMA_POOL_HITS` / `DMA_POOL_MISSES` through QUERY_STATS: misses should
  stop growing after warm-up.
* A/B: `NvSpinUs=0` against the default, same boot. Expect no change in the
  counters other than the two above.
* Registry write count: `NvIoctl` should advance in the registry at most four
  times a second while forwarding, and Open+Close should no longer show a
  millisecond-class step.

## 6. NOT implemented (risky blind), best expected gain first

1. **MSI-X for the control queue**: DONE as an opt-in (`MsiMode=2`; INF `MSISupported=0`, INTx
   fallback and counters; `docs/msi-interrupts.md`). Not verified on hardware: the
   per-call gain (est. 20-35 us from the host side) is measured by `NvRttMeanUs` and
   the `NvRttB*` histogram, A/B against `MsiMode=1`.
2. **Lean DPC for Raw completions** (est. 2-5 us on the completion side, and
   less `virtio_lock` contention with the waiter's next call): the DPC takes the
   lock 6-8 times (`interrupt.rs::drain_used_and_complete`) and the first hold
   already woke the waiter, so the rest delays only the NEXT forward. Folding
   them into one hold, or skipping them when the only completions were Raw, is
   NOT safe blind: that function is also the PASSIVE-callable catch-up for the
   scanout worker and the target of `request_wddm_completion_dpc`, whose queued
   DPC coalesces with an ISR's, so "nothing relevant completed" cannot be told
   from the completions alone. It needs a `dpc_work_requested` flag set by every
   other producer of work (bind promotion, refresh, WDDM ready queue) and then
   the skip becomes provable; the lock order notify -> virtio also forbids
   merging the notify-scope holds. Also: `DxgkCbNotifyDpc` runs BEFORE the drain
   today; moving the first drain ahead of it would wake the waiter earlier
   (est. 1-2 us) but the ordering is documented as deliberate.
3. **Claim the reply from the DMA buffer** (saves one memcpy, and the DISPATCH
   copy under the lock; for a 64 KiB reply est. 5-10 us held under
   `virtio_lock`, which stalls every other CPU's transport access): the waiter
   would take the parked entry back by identity and copy at PASSIVE. Needs a
   claim protocol between `parked`, token reuse and the PASSIVE reap.
4. **Negotiate `VIRTIO_RING_F_EVENT_IDX`** (est. 1-2 us per suppressed kick and
   fewer interrupts, only with concurrent forwards): host already offers it. The
   guest must accept it and create the queue with `event_idx = true`; a bug
   there loses wakeups (hang), and it changes the Venus submit paths too.
5. **Batch or pipeline RM calls in the UMD** (up to one round trip per saved
   call): fire-and-forget for calls whose reply is not needed (`RM_FREE`),
   several ioctls per escape. Outside the KMD; protocol change.
6. **`NoAdapterSynchronization` on the NVRM escape** (unknown; potentially large
   with several threads): the KMD already counts the flag (`EscNoSy`); whether
   dxgkrnl serialises escapes per adapter without it is not established here.
   UMD-side change, to be gated on a measurement.
7. **Cache the physical address of a `DmaBuffer`** (est. 0.2-0.4 us): `Hal::share`
   calls `MmGetPhysicalAddress` for each of the four descriptors although the
   buffer is physically contiguous and its address is known. A single-entry
   cache under `virtio_lock` would do it; touches the HAL contract.
8. **Fewer descriptors per forward** (est. 0.2 us plus host work): the wire tail
   splits request and reply into header + body, four descriptors.
9. **Splice a pin's table straight into the DMA buffer** (one `Vec` per
   registration, not per call): would move `claim_nvrm_pin_deep` inside
   `raw_roundtrip` and change when a pin is claimed relative to early failures.
10. **Lock-free ownership lookup** (the handle table is a linear scan under the
    lock; negligible at today's handle counts).
