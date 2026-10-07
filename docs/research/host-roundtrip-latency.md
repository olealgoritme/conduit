# Host round trip of a Windows guest's GPU copy

Where the time goes between the Windows KMD kicking the queue with a fenced
Venus copy (the windowed-Present ring-1 copy, `BltAsync`) and the completion
interrupt reaching the guest, measured on the host only, and what the host
can do about it. The guest's own counter (`BltAsyncLat`, KMD submit to
completion DPC) shows 0.5-1 ms for 98% of these copies, while the copy itself
is about 0.2 ms of GPU time ([HANDOFF.md](../HANDOFF.md), "Where the
windowed frame time goes now").

Reference host: Ryzen 9 7950X, RTX 5090, a 16-vCPU Windows 11 guest pinned
to CPUs 8-15 and 24-31 (one CCD), QEMU 11.1 with `vhost-user-test-device-pci`,
`performance` governor, `acpi_idle` with POLL/C1/C2/C3. Load: Unigine Heaven
D3D11, 1600x900 windowed, composed by an NVK DWM, `GuestBlob=1 BltAsync=1
ForeignCopy=1`, MSI-X in the guest, about 220 fps.

## Summary

- **Host dispatch to MSI: 446 µs p50, 828 µs p99.** Of that, about 345 µs is
  the GPU and the driver: about 208 µs of copy, and the rest mostly waiting
  for the graphics engine, which Heaven also uses. **Host CPU overhead is
  about 105 µs**: about 50 µs from the kick to the copy's `vkQueueSubmit`,
  and about 50 µs from the GPU being done to the MSI, plus a few µs to pick
  up the kick.
- The copy is **bandwidth-bound**: 27.6-28.4 GB/s into guest pages, whatever
  their backing (4 KiB, THP, scattered, contiguous). The ceiling is the GPU's
  link, which trains at **PCIe Gen5 x8** ("downgraded" from x16) on the
  reference host.
- The copy runs on queue family 0, the graphics engine. Under a competing
  graphics load it waits for a timeslice: 2.3 ms against 0.3 ms on the
  transfer-only family 1 (the copy engine), which runs alongside graphics.
  **Moving the KMD's copy queue to family 1 is the largest lever left on this
  round trip.** It is a guest change.
- Host-side fixes, each behind an option, off by default: no empty interrupt
  after holding a fenced chain, one renderer round trip per fenced submit
  instead of two, two thread hops fewer on the fence's way back, a batched and
  cheaper event thread, and CPU placement. Measured: host round trip 454 →
  410 µs p50, fence return path 57.5 → 25.4 µs, queue thread per fenced submit
  89 → 37 µs ("Before and after").
- C-states do not show on this path. Every wakeup on it landed on a CPU that
  was already awake, and wake to run took 2-3 µs.

## Method

Read-only observers on the live processes: bpftrace uprobes on the backend
(`Venus::dispatch`, `IpcClient::{submit, create_fence, signalled}`,
`deliver_completions`, `VringRwLock::signal_used_queue`), on conduit-venus
(`Virgl::{submit, create_fence, signalled}`, `write_context_fence`) and on its
virglrenderer (`render_context_dispatch_submit_cmd`,
`vkr_dispatch_vkQueueSubmit`, `vkr_queue_sync_submit`,
`render_context_update_timeline`, `proxy_context_retire_fences_internal`).
Also `sched_wakeup` and `sched_switch` through raw tracepoints, filtered to
the backend, conduit-venus and QEMU, `power:cpu_idle`, and `kvm:kvm_msi_set_irq`.
A round trip is keyed by its fence id, from the guest's `SUBMIT_3D` header to
`write_context_fence`, and matched by order where virglrenderer uses its own
sequence numbers.

Two captures were taken under load: one with every probe (2319 copies in
12 s), and one with three uprobes only (1868 copies in 10 s). The two agree
within 15 µs end to end, which is the probes' own cost. The numbers below are
from the full capture, with totals from the light one.

The GPU copy alone was timed outside the guest with
`host/venus/examples/venus-guest-blob.rs`. That example makes the KMD's exact
copy, a 1600x900 BGRA optimal image into a guest-pages buffer, through its own
conduit-venus, and times it with GPU timestamps. It now takes `GB_PAGES`,
`GB_ORDER`, `GB_QF`, `GB_ITER` and `GB_LOAD` (see its header).

## The path

```text
vCPU: KMD writes the chain, notify write ── ioeventfd ──► backend vring_worker (epoll)
vring_worker: drain, Venus::dispatch
   ├─ IpcClient::submit ──socket──► conduit-venus main: Virgl::submit ─ proxy socket ─► render worker: vkQueueSubmit(copy)
   │      ◄── reply (venus-ipc reader thread, channel) ──┘
   └─ IpcClient::create_fence ──socket──► conduit-venus main ─ proxy ─► render worker: QueueSubmit(fence) → vkr-queue thread
          ◄── reply ──┘                                                 vkr-queue: WaitForFences (poll on the driver fd)
   chain held                                                                     │ GPU interrupt (CPU 0)
                                                                     vkr-queue: timeline write + eventfd
                                                proxy sync thread ◄──────────────┘
                                    write_context_fence: queue + eventfd
               conduit-venus main ◄─┘ forward FENCES ──socket──► backend venus-ipc reader: queue + eventfd
                                                             backend nvgpu-fences ◄─┘ deliver_completions
                                                             used ring, signal_used_queue ── irqfd ──► MSI into the guest
```

There are four thread hops on the way back after the GPU, and the queue
thread waits for two synchronous round trips on the way in.

## Stage breakdown (baseline, Heaven, µs)

| stage | thread | p50 | p99 |
|---|---|---|---|
| kick → vring_worker woken → running | backend | 2.3 | 4.2 |
| woken → `Venus::dispatch` (drain, read chain) | backend | 6.3 (from wake) | 11.2 |
| dispatch → IPC submit sent | backend | 2.5 | 5.3 |
| IPC submit → conduit-venus `Virgl::submit` | venus main (woken after 7.5) | 18.5 | 26.6 |
| `Virgl::submit` (proxy socket send) | venus main | 7.3 | 11.7 |
| submit reply → backend resumes | venus-ipc → vring_worker | 19.0 | 42.2 |
| between submit and create_fence | backend | 4.6 | 12.3 |
| IPC create_fence → `Virgl::create_fence` | venus main | 12.2 | 20.5 |
| `Virgl::create_fence` | venus main | 3.8 | 7.5 |
| fence reply → backend resumes | venus-ipc → vring_worker | 14.7 | 36.5 |
| **dispatch → chain held (queue thread busy)** | backend | **≈80** | |
| venus submit → render worker picks up the command | render worker | 16.8 | 23.7 |
| `vkQueueSubmit` of the copy (driver) | render worker | 10.0 | 16.1 |
| **dispatch → copy submitted to the driver** | | **≈50** | |
| copy submitted → vkr-queue woken (GPU + driver wake) | vkr-queue | **344.5** | **715.4** |
| vkr-queue woken → timeline write | vkr-queue | 5.7 | 24.0 |
| timeline eventfd → proxy sync thread woken | proxy sync | 6.0 | 15.3 |
| timeline → `write_context_fence` | proxy sync | 14.9 | 28.4 |
| fence callback → venus main forwards | venus main | 9.1 | 26.1 |
| venus forwards → backend deliverer woken (two hops) | venus-ipc → nvgpu-fences | 15.1 | 34.8 |
| fence callback → `deliver_completions` | backend | 29.8 | 66.1 |
| `deliver_completions` → `signal_used_queue` | nvgpu-fences | 6.4 | 12.6 |
| `signal_used_queue` → MSI injected (irqfd, synchronous) | nvgpu-fences | 1.4 | 3.6 |
| **timeline written → MSI (host fence return path)** | | **48.7** | **86.7** |
| **dispatch → MSI** (light capture) | | **445.7** | **827.7** |

What the guest sees beyond this happens on the guest side: the KMD's submit
to its notify write (one exit), and the MSI to the ISR to the DPC.

### The 345 µs "GPU + driver" stage

- The vkr-queue thread waits in NVIDIA's `vkWaitForFences`. Per fence it
  makes one `poll` (10 ms timeout) that returns at once and one that blocks
  for 256-512 µs and ends with the GPU interrupt. The NVIDIA MSI-X vectors are
  on CPU 0, and the thread is woken from there. There is no timer polling and
  no 10 ms step (that was the exportable sync fence, fixed by patch 0001).
- The copy itself takes 203-208 µs, the same on every queue family, every
  backing and every page order (table below). The remaining 130-140 µs are
  waiting for the graphics engine, plus the interrupt and wake.

### Copy bandwidth and the queue family

| guest pages | order | queue family | GPU copy p50 / p90 µs | GB/s |
|---|---|---|---|---|
| 4 KiB (`MADV_NOHUGEPAGE`) | scattered | 0 | 208.4 / 208.6 | 27.6 |
| 4 KiB | contiguous | 0 | 208.4 / 208.6 | 27.6 |
| shmem THP | scattered | 0 | 208.4 / 208.5 | 27.6 |
| shmem THP | contiguous | 0 | 202.9 / 203.8 | 28.4 |
| either | either | 1 (copy engine) | 202.6-203.2 / 203.1-203.5 | 28.4 |

Hugetlb was not measured because no huge pages were reserved. There is no
per-page or IOMMU cost: the IOMMU is in DMA-FQ mode for the GPU, and 4 KiB
scattered pages copy as fast as 2 MiB ones. 28 GB/s is what a Gen5 x8 link
carries. Reserved huge pages for the guest ([HOST-TUNING.md](../HOST-TUNING.md))
help elsewhere (TLB, no swapping), but they will not shorten this copy. A
link at x16 would roughly halve it.

| copy queue | competing GPU load | GPU copy p50 µs | submit to signal, p50 / p90 µs |
|---|---|---|---|
| 0 (graphics) | none (the VM's desktop only) | 208.2 | 307.8 / 503.7 |
| 1 (transfer) | none | 203.2 | 292.0 / 309.0 |
| 0 | graphics, 2.5 ms batches | 208.3 | 2317.5 / 2645.5 |
| 1 | graphics, 2.5 ms batches | 204.4 | 306.1 / 327.5 |
| 2 (compute) | graphics, 2.5 ms batches | 255.5 | 2367.4 / 2641.0 |

The host's families are 0 (graphics, compute, transfer; 16 queues), 1
(transfer only; 2 queues) and 2 (compute and transfer; 8 queues). The KMD
creates its Venus device and the ring-1 queue on family 0
(`kmd_render/src/virtio/venus/bringup.rs`). On families 0 and 2 the copy is
timesliced with every other context on the graphics engine. In the guest
that means Heaven's NVK channel and DWM's. On family 1 it runs on a copy
engine, concurrently. The guest change is to create the ring-1 queue on
family 1 and check the queue-family ownership of the source image and the
guest-blob buffer (`VK_SHARING_MODE_CONCURRENT`, or ownership transfers).

## Other findings

- **An empty interrupt per fenced submit.** The queue handler marks a kick
  as having used the ring when it held a fenced chain, so it signals the guest
  although nothing is on the used ring. This shows in the trace as an MSI from
  the queue thread right after every fenced `SUBMIT_3D`. The guest takes an
  interrupt, finds nothing, and the real completion follows 0.4 ms later.
- **The event thread** (`nvgpu-events`, the RM event relay for NVK fences) is
  a cost on the NVK path, not on this one:
  - it signals the guest once per event: about 11,000 MSIs a second under
    Heaven, often 10 back to back about 4 µs apart;
  - its safety-net sweep calls `poll` once per watched descriptor every
    millisecond: about 90,000 system calls a second on an idle desktop (5% of
    a core), and 500,000 a second with about 400 descriptors.
- **Placement.** The backend and conduit-venus threads float over all 32
  CPUs. Most of their wakeups land on the guest's CPUs (31, 15, 9, 14, 29, ...),
  because those are idle while the vCPUs halt. The wake itself costs 2-3 µs.
  The cost is the preempted vCPU and a cold L2.
- **C-states.** `acpi_idle` offers C3 (350 µs exit latency in its table). None
  of the traced wakeups on this path paid an idle exit: they are TTWU IPIs to
  a CPU that was already up, and wake to run took 2-3 µs.
- **The fence pump's 1 ms nap.** When the pump is woken but another thread
  already returned the chains, it sleeps 1 ms. A fence that arrives during
  that sleep waits for the 2 ms poll timeout or the next kick. This is
  probably part of the 66 µs p99 above.

## Options (off by default)

`conduit config set backend.latency all` (or a list) and `conduit config set
backend.cpus 0-7,16-23`. Both apply when the VM's backend next starts.

| option | where | what |
|---|---|---|
| `quiet-held` | backend | a kick that only held fenced chains does not interrupt the guest |
| `fused-submit` | backend, conduit-venus (`FEATURE_SUBMIT_FENCED` bit 3, IPC op 15) | a fenced `SUBMIT_3D` is one renderer call, `submit_fenced`, instead of `submit` and `create_fence`: one round trip less while the queue thread waits (about 33 µs) |
| `direct-fences` | conduit-venus `--direct-fences`, backend | conduit-venus sends each fence from the virglrenderer thread that retires it, under one send lock with the serve loop, instead of through the serve loop. The backend's renderer reader returns the chains itself, through `Renderer::set_fence_hook`, when the backend lock is free, and otherwise leaves them to the fence pump as before. Two hops fewer on the way back |
| `event-batch` | backend | the event thread signals once per pass and sweeps with one `poll` over all descriptors |
| `fence-spin` (named only, not in `all`) | conduit-venus `--fence-spin-us 150`, virglrenderer patch 0004 | vkr's fence threads poll around a fence's expected completion (phase 3) |
| `backend.cpus` | backend and conduit-venus `--cpus` | every thread of both stays on the given CPUs (the host's CCD, away from the vCPUs) |

`direct-fences` without the backend lock: the queue thread holds the lock
while it waits for a renderer reply, and that reply arrives through the same
reader thread. So the hook only ever tries the lock. A fence that arrives
before its `create_fence` reply (possible once fences no longer queue behind
the reply) is not taken until the queue thread has recorded the chain and
released the lock.

## Before and after

Same host, the same Heaven load (windowed 1600x900 on a 5120x1440@240
desktop, `GuestBlob`/`BltAsync`/`ForeignCopy`), one VM cycle per row:
A `backend.latency off`, B `all`, C `all` plus `backend.cpus 0-7,16-23`.
In all three rows the guest ran on **INTx**: its MSI-X grant was lost after
a driver package install. So the interrupt stage here includes QEMU's main
loop raising the line (`kvm_set_irq` from QEMU's main thread, 2900-3200 a
second), not irqfd as in the baseline above. The comparison between the
rows holds. For the irqfd path, the MSI-X baseline above is the reference.

Light captures (three uprobes), µs, p50 / p99:

| stage | A off | B all | C all + cpus |
|---|---|---|---|
| queue thread woken → `Venus::dispatch` | 6.0 / 11.2 | 6.0 / 10.9 | 5.9 / 9.3 |
| dispatch → timeline written (submit, GPU, wake) | 394 / 707 | 391 / 722 | 380 / 713 |
| timeline → `write_context_fence` | 12.0 / 26.9 | 11.7 / 26.1 | 9.0 / 25.2 |
| `write_context_fence` → interrupt | 45.1 / 76.1 | 28.0 / 45.3 | 15.1 / 43.9 |
| **timeline → interrupt (fence return path)** | **57.5 / 90.6** | **40.6 / 65.5** | **25.4 / 65.9** |
| **dispatch → interrupt (host round trip)** | **454 / 762** | **433 / 776** | **410 / 739** |

Full captures (every probe; each probe adds a little to every row), µs, p50:

| stage | A off | B all | C all + cpus |
|---|---|---|---|
| dispatch → chain held (queue thread busy) | 89.3 | 59.4 | 37.2 |
| dispatch → copy's `vkQueueSubmit` returned | 54.9 | 51.5 | 31.0 |
| copy submitted → NVIDIA interrupt (GPU) | 327.7 | 328.5 | 328.3 |
| NVIDIA interrupt → vkr-queue running | 18.5 | 16.1 | 23.2 |
| `signal_used_queue` → QEMU raises INTx, p50 / p99 | 13.2 / 270 | 13.2 / 996 | 11.5 / 1582 |

Guest side, same rows (the KMD's `BltAsyncLat` histogram over about 11 s,
submit to completion DPC; driver 346.1, INTx):

| | A off | B all | C all + cpus |
|---|---|---|---|
| copies < 500 µs | 0.3 % | 1.1 % | 5.2 % |
| copies 0.5-1 ms | 99.1 % | 98.0 % | 94.5 % |
| copies > 1 ms | 0.6 % | 0.9 % | 0.3 % |
| producer deferral (`BltDeferUs / BltAsyncDefer`) | 753 µs | 654 µs | 767 µs |

The guest's histogram has 250 µs buckets, so a 44 µs gain shows only as
copies crossing the 500 µs edge: 7 → 27 → 135 of about 2450. Nearly every
copy is between 500 µs and 1 ms in the guest, against 410-450 µs from host
dispatch to interrupt. The rest of the guest's time is its submit, its kick,
and the INTx interrupt through to the DPC.

Reading:

- The options cut the host's own part of the round trip:
  - the fence return path from 57.5 to 40.6 µs (`direct-fences`), and to
    25.4 µs with the threads on the host's CCD;
  - the queue thread's time per fenced submit from 89 to 59 µs
    (`fused-submit`), and to 37 µs pinned;
  - the copy reaching the driver 24 µs sooner when pinned.
  End to end: 454 → 433 → 410 µs p50, 44 µs (10%) off the host round trip.
  The GPU part (328 µs from the copy's submit to the NVIDIA interrupt) does
  not move. The next step there is the copy-engine queue.
- `quiet-held`: interrupts raised by QEMU went from 3210 to 2911 a second
  (B), about one fewer per fenced submit.
- The tails come from INTx. The p99 of the step where QEMU's main loop
  raises the line grows from 270 µs (A) to 1.6 ms (C) in the full captures.
  In row C the backend and conduit-venus share CPUs 0-7,16-23 with QEMU's
  main loop, emulator thread and iothread, which libvirt pins there too,
  and the full capture's probes add load on the same CPUs. With MSI-X the
  interrupt is an irqfd write from the backend thread and QEMU's main loop
  is not on the path. Under INTx, keep `backend.cpus` off QEMU's emulator
  CPUs, or leave it unset.
- Recommendation: make `quiet-held`, `fused-submit` and `direct-fences` the
  default once a soak is clean. Re-measure `backend.cpus` with MSI-X before
  recommending it. `event-batch` matters for the NVK event path, not this one.

## Phase 3: observing the fence sooner

The frame stage tracer (driver 346.1, windowed Heaven) puts 119 µs p50
(488 p99) between "vkr's submit done" and "fence signalled" once the GPU copy
time (timestamps, 234 µs) is subtracted. The question was whether
conduit-venus or virglrenderer could learn of the fence sooner than NVIDIA's
`vkWaitForFences` tells it.

`host/latency/fencewake.c` measures it without a guest. The GPU writes a
marker into host memory right after the copy, a thread spinning on that
memory notes when it lands, and the waiter's return time minus the marker's
is the wake latency. Same 5.76 MB copy into host memory, 300-400 copies per
row. The VM's desktop was running on the same GPU.

| wait method | queue family | submit → GPU done, p50 / p90 µs | GPU done → waiter returns, p50 / p90 / p99 µs |
|---|---|---|---|
| `vkWaitForFences` (what vkr does) | 0 | 316 / 408 | 14.7 / 20.2 / 55.8 |
| `vkGetFenceStatus` loop | 0 | 316 / 357 | 1.6 / 2.7 / 12.0 |
| sync file (exported fence), `poll` | 0 | 316 / 335 | 18.2 / 27.8 / 514 |
| timeline semaphore, `vkWaitSemaphores` | 0 | 314 / 371 | 14.5 / 20.8 / 46.6 |
| sleep to the expected end, then poll (`hybrid`) | 0 | 312 / 323 | 1.8 / 12.0 / 15.8 |
| `vkWaitForFences` | 1 (copy engine) | 221 / 230 | 13.2 / 18.8 / 548 |
| `vkWaitForFences` | 2 (compute) | 305 / 398 | 15.8 / 20.7 / 644 |
| sleep to the expected end, then poll (`hybrid`) | 1 | 221 / 223 | 1.0 / 1.2 / 5.1 |

The GPU copy itself took 221-224 µs on families 0 and 2 and 205 µs on
family 1.

- **The wake is about 13-15 µs, not 119.** NVIDIA's wait blocks in `poll` on
  its device file and is woken from its interrupt handler on CPU 0. In the
  live trace, the NVIDIA interrupt to the vkr-queue thread running was
  16-23 µs p50. A sync file (worse tails) or a timeline semaphore (the same)
  does not beat it. Only polling does: 1-2 µs.
- **The rest of the 119 µs is the copy waiting to start.** On the graphics
  engine (families 0 and 2), submit → GPU done exceeds the copy by about
  90 µs even with the host otherwise idle. On the copy engine (family 1) it
  exceeds it by 16 µs. Under Heaven the wait for a graphics-engine timeslice
  grows (the 2.3 ms against 0.3 ms microbenchmark above). The fix for that
  part is the KMD's copy on a transfer-only queue, not a faster fence wait.
- C-states play no part: holding `/dev/cpu_dma_latency` at 0 moved the
  `vkWaitForFences` wake from 13.2 to 11.4 µs p50 and did not change the
  tails.

**Fence spin** (`patches/0004-vkr-queue-fence-spin.patch`, off by default):
vkr's sync thread sleeps until 30 µs before the fence's expected completion
(the second shortest of its queue's last 16 queued-to-signalled times), then
polls `vkGetFenceStatus` for at most `CONDUIT_VKR_FENCE_SPIN_US`
microseconds, then waits in the driver as before. It is set by
`conduit-venus --fence-spin-us N` and by `conduit config set backend.latency
<list>,fence-spin` (150 µs; not part of `all`). The patch applies after
0001-0002 and after 0003.

Measured through a private conduit-venus with the guest-blob example
(family 1, 300 copies, two alternations each):

| | wall p50 µs | wall p90 µs | `poll` calls by the sync thread |
|---|---|---|---|
| off | 298.8, 297.9 | 398.8, 400.1 | 470, 458 |
| fence spin 150 µs | 294.4, 297.2 | 389.0, 376.6 | 27, 30 |

The spin engages (the sync thread's blocking `poll` calls drop by 94%), yet
the end-to-end gain is small: 1-4 µs p50 and
10-24 µs p90. The other hops after the sync thread dominate (the proxy
thread, conduit-venus, the backend: 25-41 µs with the options above). It
costs 30-90 µs of CPU time per fence, about 1-2% of a core at 250 fences a
second. Verdict: worth a row in the next guest A/B, but not a default. The
copy-engine queue is the lever for this stage.

## Reproducing

The capture scripts generate a bpftrace program for the live PIDs and symbols
(uprobes need the unstripped binaries the packages ship) and pair the events
by fence id. The light capture uses only `Venus::dispatch`,
`render_context_update_timeline`, `write_context_fence`, `sched_*` and
`kvm_msi_set_irq`, and costs under 15 µs per round trip. Keep a capture
under 30 s: the full one writes about 80,000 lines a second under Heaven.
