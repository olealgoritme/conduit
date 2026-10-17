# Windows 11 guest vs bare metal: performance gaps and plan

Scope: the D3D11, D3D12, Vulkan and GL frame path in a Windows 11 guest on
the RTX 5090 (win11 at 5120x1440@240), guest driver 22.22.405.84 with code
defaults, host from main with its defaults. Code references are to main.
Every guest knob and its default: [WINDOWS-KNOBS.md](WINDOWS-KNOBS.md); host
settings: [HOST-TUNING.md](HOST-TUNING.md).

## Measured

| Workload | Result |
|---|---|
| Counter-Strike 2, 1920x1080 | ~330-380 fps |
| Counter-Strike 2, 5120x1440, with bots | ~350 fps |
| Counter-Strike 2, lower resolutions | ~400-500 fps |
| 3DMark Steel Nomad (DX12) | score 5823 |

## Configuration that matters for the numbers

All of these are defaults; nothing is set by hand.

| Setting | Value | Effect |
|---|---|---|
| backend `backend.latency` | `all` | quiet-held, fused-submit, direct-fences and event-batch on |
| `backend.gpu_timeslice_us` | 100 | host display work waits at most one short slice behind the guest's GPU work |
| `host.isolation` | `auto` | host work, device interrupts and kernel workqueues off the VM's CPUs |
| KMD interrupts | MSI-X (`MsiMode` 2) | with the boot-loop breaker and the delivery latch falling back to INTx |
| windowed copy | `RedirVram` 1, `BltAsync` 1, `ForeignCopy` 1; `RmCopyEngine`, `GuestBlob`, `CopyQueue` 0 | GDI redirection textures in RM video memory, written on the KMD's RM copy-engine channel; other destinations take the Venus copy |
| flips | `ForeignFlip` 1 (`FlipAnnForeign`, `FfAsyncWin` 2 follow it), `IndepFlip` 1, `DirectFlipSupport` 5, `FlipQueueN` 1 | full-screen flip-model swap chains are scanned out from their own buffers; windowed apps are DWM-composed |
| GDI | `GdiAccel` 0 | no GDI hardware acceleration |
| D3D11 front end | `UmdThreadedFrontend` 1 | the immediate-context calls of every process except dwm.exe are recorded into a ring and replayed on a worker thread |
| ICD policy | `DwmIcd=nvk` (install.ps1), `Icd` absent | DWM and apps on NVK; Venus is loaded as the fallback ICD |
| input | virtio-input ([INPUT.md](INPUT.md)) | relative mouse and keyboard reports straight into the guest's event queues |

## 1. The path, hop by hop

### 1a. Per submission (vkQueueSubmit → GPU), all APIs

| # | hop | where |
|---|---|---|
| S1 | NVK writes GPFIFO entries, push, semaphore release + `NON_STALL_INTERRUPT` (+ a WFI retire release per flush) | `patches/0006` (`nvkmd_rm_exec_ctx_exec/flush`), `patches-common/0019` |
| S2 | GP_PUT to USERD, doorbell token to the usermode region | `patches/0006`; the doorbell store goes through the shared window (one KVM memslot); whether it exits is unmeasured |
| S3 | Mesa submit thread, if a wait-before-signal pattern switched the queue to threaded mode | `vk_queue.c` |
| S4 | D3D12 only: HE12 Render record per ExecuteCommandLists + KMD admission. A batch reaches NVK once its admission event fires, i.e. once the packet two ahead completed; that packet completes when its ECL fence fire reaches the KMD (W1-W4), or earlier on a completion hint from the engine worker (`Nvk12CompletionHint`, default 0). `Nvk12AdmitBeforeRender` (default 1) admits a batch one packet earlier | `umd12/src/forward12/nvk12.rs`, `queue.rs`, `ecl_hint.rs`; dxgkrnl keeps at most 2 packets in flight per node |

Submission has bare-metal shape for D3D11, Vulkan and GL: no syscall and no
host hop per submit. D3D12 pays the WDDM software-scheduler packet pipe.

### 1b. Per GPU→CPU wait (fences, timelines, DXVK and vkd3d frame-latency waits)

| # | hop | where |
|---|---|---|
| W1 | GPU semaphore release + non-stall interrupt | nvidia.ko |
| W2 | RM OS-event fd readable → the backend's event pump (epoll); a fence already reached is also found by the 1 ms sweep | `conduit-backend.rs` `event_pump` |
| W3 | `EventReady` into one of 16 posted event buffers → used ring + MSI; with `NVGPU_F_EVENT_PACKED` the reports of one pump pass share buffers and raise one interrupt | `EventReports::push_packed`, `end_pass` |
| W4 | guest ISR → DPC → `KeSetEvent` on the shared per-device non-stall event | `kmd_render/src/virtio/gpu/nvrm_events.rs`, `ddi/interrupt.rs` |
| W5 | librmclient waiter wakes and wakes the other waiters (wake generations); each re-reads its semaphore | `guest/rmclient/src/transport_windows.c` (`crm_win_event_wait_gen`), `patches-common/0040`, `0042` |

Every release wakes every waiter of the device; bare metal wakes one waiter
per semaphore in the kernel.

### 1c. Per present, D3D11 windowed (DWM-composed)

| # | hop | where |
|---|---|---|
| P1 | the UMD's copy into the DXGI destination | skipped (`NvkSkipBltCopy` follows `RedirVram`) |
| P2 | `Flush()` + the present gate in SUBMITTED mode: the app thread blocks until DXVK's CS and submit threads have drained the frame | `umd/src/forward/present.rs` → `dxvk_bridge.cpp` `present_frame_gate` → `HeliosWaitFrameSubmitted` |
| P3 | RM fence for the frame: an extra queue signal + `SEMSURF_FENCE_CREATE` escape (one host round trip) | `patches-windows/0030` |
| P4 | D3DKMT Present → `DxgkDdiPresent` Blt arm → `RedirVram` / copy-engine / BltAsync decision | `kmd_render/src/ddi/display.rs`, `ddi/vram_redirect.rs` |
| P5 | copy-engine route: GPFIFO + doorbell on the KMD's RM CE channel | `ddi/ce_present_route.rs`, `virtio/rm_client/ce_channel.rs` |
| P5' | fallback for CPU-visible destinations: the Venus copy, with the KMD worker deferring until the producer retires | `display.rs` `service_windowed_blt`, `virtio/venus/present.rs` |
| P6 | copy completion is polled: a settle spin after dispatch, then the HPD worker every 0.5 ms; no completion interrupt | `ce_present_route.rs` `settle_after_dispatch`, `kmd_logic/src/ce_route.rs` `POLL_DUE_100NS` |
| P7 | the Present's DMA fence → `signal_dma_completed` | `ddi/interrupt.rs` `drain_used_and_complete` |
| P8 | read-ledger retire → `KeSetEvent` on every registered event (up to 64) | `adapter/read_ledger.rs` `flush_broadcast` |
| P9 | dxgkrnl: the window's next Blt waits for the previous Blt packet to retire | dxgkrnl |
| P10 | the fence's `Close` after it fired (a second RM round trip, on the HPD worker) | [rm-fence-marker.md](../guest/windows/docs/rm-fence-marker.md) |
| P11 | DWM composes (its own NVK frame), then `ForeignFlip` (asynchronous, up to 2 in flight) | `virtio/foreign_flip.rs` |
| P12 | backend `ScanoutFlip` → cached dma-buf → the viewer → compositor (or direct scanout in fullscreen) | `host/backend/device`, `host/viewer`; zero-copy |
| P13 | vsync to dxgkrnl: a guest high-resolution timer at the mode's rate, free-running against the host vblank | `adapter/kobj.rs` `service_vsync_tick` |

### 1d. Per present, full-screen / scanout path (Vulkan, D3D11 independent flip, D3D12 fenced scanout)

| # | hop |
|---|---|
| F1 | present gate SUBMITTED (D3D11), or the ECL fence (D3D12) |
| F2 | `SEMSURF_FENCE_CREATE` + `SCANOUT_PRESENT` with `RM_FENCE` (two escapes) |
| F3 | the KMD queues the flip; the worker sends `ScanoutFlip` when the fence's `EventReady` arrives (the W chain) |
| F4 | `vkAcquireNextImage`: `SCANOUT_STATUS`, woken by the host's `ScanoutReleased` event |
| F5 | with independent flip the frame goes to scanout without a DWM composite; `MaxQueuedFlipOnVSync` is 1 (`FlipQueueN`) |

### 1e. Per RM call (allocations, maps, fences, flips, KMD services)

Guest `escape_nvrm` → `nvrm_forward` → `ctrl.rs` `raw_roundtrip`: lock holds,
a kick, a short adaptive spin (`NvSpinUs`), then a KEVENT. On the host the
backend's control-queue handler, serialised on one `Mutex<NvidiaBackend>` for
all guest processes, does one `ioctl`, then the used ring and MSI → ISR →
DPC → `KeSetEvent`. The steady-state rate is low; this matters for present fences,
flips, allocation bursts (level loads, map changes) and deferred frees.

### 1f. Venus fallback path

Processes on the deny-list: app → Venus ICD → KMD ring → backend → IPC to
conduit-venus → virglrenderer → host NVIDIA Vulkan; every fenced submit is a
host round trip. Still on Venus in the KMD: the windowed-copy fallback for
CPU-visible destinations, paging copies, the present ready-poll, the Venus
scanout path when `ForeignFlip` is 0, and GuestBlob.

### 1g. Background guest costs

| item | where |
|---|---|
| registry mirror of the KMD counters (thousands of values per full dump) | `ddi/mirror_thread.rs` |
| vsync timer DPCs, one per refresh | `adapter/kobj.rs` |
| event relay at idle, and overflow of the 16-entry event ring (`NvEvFull`) | `nvrm_events.rs` |
| GDI jobs on the GDI thread when `GdiAccel` is on | `ddi/gdi_thread.rs`, `gdi_exec.rs` |

### 1h. CS2 render-thread profile (measurement)

Measured on driver 405.42 with the D3D11 front end inline on the app thread
(`UmdThreadedFrontend=0`) and the backend latency options off, CS2 at ~150 fps: a
10-minute play recording plus a 3,000-sample instruction-pointer profile of
the busiest thread (`guest/windows/tools/thread-sample.ps1`), symbolized
against the PDBs.

- Not GPU-bound: SM busy median 46%.
- One guest thread is the frame: `D3D11RenderThread` (CS2's render thread,
  which makes every D3D11 call) at 99.7% of a CPU.
- Render-thread samples: `helios_umd.dll` (DXVK's D3D11 front end + the DDI
  forward layer) 46%, of which the `Map(WRITE_DISCARD)` path about 17% and
  binding about 8%; `d3d11.dll` 21%; game code 21%; `ntdll.dll` 9% (waking
  and waiting on DXVK threads); `win32u.dll` (D3DKMT) 2.3%.
- The hottest instructions are loads right after a pointer chase: the thread
  is bound by memory latency more than by instructions, which is why guest
  RAM on huge pages matters ([HOST-TUNING.md](HOST-TUNING.md#memory-never-swap-guest-ram)).

On bare metal NVIDIA's UMD defers most of its work to driver threads; the
threaded front end
([d3d11-threaded-frontend.md](../guest/windows/docs/d3d11-threaded-frontend.md))
does the same for our UMD.

## 2. Gaps vs bare metal, ranked by expected gain

Bare-metal reference behaviour: NVIDIA on Windows has HAGS hardware queues,
monitored fences written by the GPU and seen through MSI-X, flip queues of
2-3, MPO and independent flip, and real vblank interrupts. On Linux,
NVIDIA's userspace submits through USERD/doorbell, waits through
per-semaphore `NV_SEMAPHORE_SURFACE` waiters (one wake per waiter, in the
kernel) and presents through KMS with plane fences.

| rank | gap | where we pay it |
|---|---|---|
| G2 | the SUBMITTED present gate on the app's render thread | 1h, P2/F1; CPU-bound titles (CS2) |
| G3 | the GPU→CPU wake chain wakes every waiter, through a 16-buffer event ring and a 1 ms sweep | 1b; DXVK frame pacing and vkd3d fence waits |
| G4 | windowed copy completion is polled; the Venus fallback defers on the CPU | P6, P9: dxgkrnl serialises the next Blt behind it |
| G5 | two RM round trips per present fence (create + close), the close on a serial worker | P3, P10 |
| G6 | D3D12's packet pipe (no hardware queues, at most 2 packets in flight) | S4 |
| G8 | RM call round trip: one backend mutex for every client, KMD lock holds, a shared control queue without `EVENT_IDX` | 1e: level loads and stutters |
| G9 | synthetic vsync not locked to the host vblank; flip queue depth 1 | P13, F5: pacing and latency, not throughput |
| G10 | read-ledger broadcast to every registered event | P8: grows with the number of D3D11 processes |
| G11 | Venus still required (`--venus`): fallback ICD, KMD copy fallback, paging, GuestBlob | 1f |
| G12 | background guest overhead (registry mirror, vsync DPCs) | 1g |

## 3. Plan

The T numbers are stable identifiers (PRs refer to them). Tags: **[V]**
removes a Venus dependency, **[F]** flip or vblank work.

### T2. A non-blocking D3D11 present
- **What.** Drop the SUBMITTED gate (`present.rs` → `present_frame_gate` →
  `HeliosWaitFrameSubmitted`): reserve the frame's RM fence value at record
  time on the CS thread, hand the KMD a (fence, value) pair it waits on
  GPU-side (copy-engine route) or worker-side (flip), and let `D3DKMT
  Present` go out at once, as DXVK queues `presentImage` to its submit
  thread.
- **Risk.** Ordering with the KMD Blt; the read ledger and the present stream
  must carry the reserved value.
- **Measure.** `NVK present timing gate=`, `present-gate avg_us`, PresentMon
  `MsInPresentAPI`, `HELIOS_VK_FRAMETIME`, StageTrace.

### T3. Per-waiter GPU wakes instead of one shared non-stall event per device
- **What.** A wake source per NVK sync or waiting thread (an RM
  `NV_SEMAPHORE_SURFACE` waiter mapped to a per-waiter KMD event); replace
  the wake-all generations in `crm_win_event_wait_gen` and the 1 ms poll
  (`patches-common/0040`); grow the KMD event ring past 16 buffers
  (`nvrm_events.rs` `EVENT_QUEUE_SIZE`, after moving the queue state off the
  boot stack); sweep the semaphore on the fd edge instead of every 1 ms. The
  host waiter path exists for Linux guests ([SYNC.md](SYNC.md)).
- **Effort.** Large: NVK, rmclient, KMD and backend.
- **Measure.** `NVK_WAIT_STATS=1`; `NvEvFull`/`NvEvMaxP`; MSIs/s;
  `conduit trace VM latency`.

### T4. Event-driven completion for the KMD copy-engine route; retire the Venus copy fallback [V]
- **What.** The CE push ends with a semaphore release +
  `NON_STALL_INTERRUPT` on an RM event the KMD registered, so `EventReady` →
  DPC completes the WDDM fence directly, replacing the settle spin and the
  0.5 ms worker poll. Replace `ce_vram`'s 1 ms sleeps in its waits with a
  high-resolution timer or the event. Then move the remaining windowed copies
  to the channel and drop the Venus copy, the producer deferral
  (`service_windowed_blt`) and GuestBlob.
- **Risk.** The WDDM fence-ordering invariants (`zero-copy-present.md` 24.4).
- **Measure.** `StageTrace=1` + `stages.sh`; `CeRtDoneUs`, `CeRtPollUs`;
  DxgKrnl ETW 41→42 on the app thread.

### T5. One RM round trip (or none) per present fence
- **What.** Pre-create a ring of present fences per swapchain or device (as
  the D3D12 UMD does for ECL fences, `helios_vkd3d_prepare_ecl_fence`) and
  reuse them by value on one semaphore surface; that removes
  `SEMSURF_FENCE_CREATE` from `patches-windows/0030` and the `Close` from the
  KMD worker. Alternative on the host: create+wait in one message, close on
  signal without a guest message.
- **Risk.** Fence value reuse against the KMD marker bookkeeping
  ([rm-fence-marker.md](../guest/windows/docs/rm-fence-marker.md)).
- **Measure.** `conduit trace --summary` (RM calls per frame); `NvRttN`;
  `NVK present timing finish=`/`total=`.

### T6. Deeper flip queue [F]
- **What.** `MaxQueuedFlipOnVSync` 2 (`FlipQueueN`) together with flip
  completion from the host (`FlipDoneHost`), for independent-flip games.
- **Measure.** PresentMon `PresentMode`; `IfN`/`IfGap*`, `FlipLat*`.

### T7. Read ledger: signal only the owner, after dropping the locks
- **What.** `adapter/read_ledger.rs` `flush_broadcast`: record the waiting
  registration per slot and signal only it, with `KeSetEvent` after the
  transport lock is released (or from a DPC).
- **Measure.** `AqSigB` per retire → ~1; DPC time; `SubLock`.

### T8. D3D12: off the 2-packet software queue
- **What.** WDDM hardware queues (`CreateHwQueue`/`SubmitCommandToHwQueue`,
  `ddi/scheduler.rs`, which answers `STATUS_NOT_SUPPORTED`) with monitored fences
  backed by GPU-written RM semaphores.
- **Effort / risk.** Large, high: a new WDDM model in the KMD.
- **Measure.** `umd12-*.log` frame split (ECL CPU wait, commit→admission);
  DxgKrnl queue-packet ETW.

### T8a. D3D12: a shorter admission round trip
- **What.** Two UMD knobs. `Nvk12AdmitBeforeRender` (default 1) queues a batch's
  admission ahead of its packet, so three batches can be on the GPU instead of
  two. `Nvk12CompletionHint` (default 0; µs of polling per admission wait)
  lets vkd3d's queue worker read each batch's producer timeline in the guest
  and name the batches it saw finish to the KMD (`HELIOS_ESCAPE_ECL_DONE`), which
  completes their packets without the host leg of the ECL fence (W1-W4):
  [rm-fence-marker.md](../guest/windows/docs/rm-fence-marker.md#completion-hints).
- **Risk.** Low: both keep the runtime's ordering (waits before the ECL still
  hold the batch, signals after it still follow the work); a hint can only
  complete the hinting process's own packets. The poll costs a core while a
  worker waits.
- **Measure.** `d3d12_gpu_bound -t submit` (GPU idle gap per ExecuteCommandLists,
  commit-to-admission, hints fired/late, done-to-admission); KMD `E12Hint*`;
  Steel Nomad D3D12.

### T9. A leaner completion DPC
- **What.** Collapse the lock holds in `interrupt.rs`
  `drain_used_and_complete` into one pass, and signal all ready WDDM fences
  in one `DxgkCbNotifyInterrupt` batch instead of one `take_one_ready_wddm`
  SyncExec + DPC requeue per fence.
- **Measure.** `NvRtt` histogram; `CeRtDoneUs`; xperf DPC time.

### T10. Shorten the RM call path (backend and KMD)
- **What.** Backend: split the control-queue dispatch so RM calls of
  different clients do not serialise on one `Mutex<NvidiaBackend>`; turn on
  `EVENT_IDX` on the KMD side (`virtio/gpu/mod.rs`, `event_idx` false). KMD:
  fewer lock holds per enqueue; a second control queue for RM traffic so
  flips and copies do not queue behind slow RM calls.
- **Measure.** `NvRttB*`/`NvRttMeanUs`; host trace `queue_us` vs `host_us`.

### T11. Host-paced vsync and flip-done from the host [F]
- **What.** The backend forwards the viewer's `wp_presentation` feedback as a
  vblank stamp and `service_vsync_tick` locks phase to it; flips retire on
  the real present (`FlipDoneHost`). Design:
  [host-vblank-pacing.md](../guest/windows/docs/host-vblank-pacing.md); PR
  #78 (draft).
- **Measure.** `VsLate*`, `FlipHostLat*`, `FlipLat*`, PresentMon display
  latency.

### T12. Remove the HPD worker's head-of-line blocking
- **What.** Move flip programming and `ForeignFlip` off the single PASSIVE
  HPD worker (`ddi/hpd.rs`), which also runs Blt dispatch, CE settle, RM
  fence closes and the RM client; give flips a dedicated high-priority
  thread. Keep GDI on `gdi_thread.rs`.
- **Risk.** Lock order scanout → venus → virtio.
- **Measure.** `HpdBusyUs`, `HpdPassMaxUs`, `FlipPrgLat*`, `FfRtt*`.

### T13. Cut background guest overhead
- **What.** The registry mirror writes only changed values, in a binary block
  per subsystem, and dumps in full on request (an escape for tooling).
  `StageTrace` stays the per-frame source.
- **Measure.** `HpdDumpUs`/`MirLastUs`.

### T14. Venus removal (S6d) [V]
- **What.** After T4, the order in
  [HANDOFF.md](HANDOFF.md#removing-venus-what-still-uses-it): KMD waits on RM
  fences (paging, present ready-poll); deny-list down to DXR; the Venus
  scanout path removed; `--venus`, conduit-venus, the virglrenderer patches
  and `--venus-guest-blobs` dropped.
- **Measure.** Venus dispatch count → 0 in `conduit trace`.

### T15. GDI path
- **What.** GDI acceleration on the copy engine (`GdiAccel`, default 0,
  [vram-redirection.md](../guest/windows/docs/vram-redirection.md) 10):
  more copy-engine coverage and batching per RenderKm, then a default.
- **Measure.** `GdiUs`, `GdiFall`, `GdiCeSub`.

Smaller items:
- **`CopyQueue=1` [V-interim].** The Venus fallback copy on the transfer
  family; only matters while that fallback is used.
- **Doorbell exit.** Measure whether the USERD doorbell store exits; if it
  does, check KVM's handling of that slot (`SubFKick` is the comparison).
- **`patches-common/0019` WFI retire release.** One WFI per flush; a release
  without WFI on a separate semaphore once the DXVK free bug is found.
- **Mesa threaded-submit switch.** Advertise wait-before-signal handling so
  vkd3d does not push the queue into permanent submit-thread mode.

## 4. What to measure first

1. StageTrace on a windowed game on the copy-engine route (`StageTrace=1`,
   `stages.sh VM 10`): the per-stage table for P2-P9.
2. `NVK_WAIT_STATS=1` + `HELIOS_VK_FRAMETIME=1` on CS2 at 1920x1080: frame
   time split into present gate, waits and GPU.
3. `conduit trace VM latency` for 5 s under the same load: the event-thread
   interrupt rate and the Venus round trip ([TRACING.md](TRACING.md#latency-capture)).
4. xperf DPC/ISR in the guest for T7/T9.

Measure at 1920x1080@240 first, then 5120x1440@240, and keep captures under
30 s.
