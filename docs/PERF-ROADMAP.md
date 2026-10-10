# Conduit Windows 11 guest vs bare metal: performance gap analysis

Scope: the D3D11, D3D12, Vulkan and GL frame path in the win11 VM on the RTX 5090. Covers driver 22.22.405.2x on the
guest side, and on the host the deployed backend from main plus host405. Code references are to origin/main (b69e7349) unless a branch
is named. None of the open branches (feat/gpu-tray, feat/guest-control, fix/text-glyphs, build/driver-405.27) changes the hot path.
fix/text-glyphs only adds a CPU fallback in `gdi_exec.rs`. 405.27 adds the DXVK 1 ms event poll (0015/0016).

## 0. Method and evidence

- **Code reads.** I read the guest NVK series (`guest/nvk-rm/patches*`, `guest/rmclient/src/transport_windows.c`), the UMDs
  (`guest/windows/umd`, `umd12`, `umd/bridge/dxvk_bridge.cpp`), the KMD (`guest/windows/kmd_render`, `kmd_logic`) and the host
  (`host/backend/device`, `host/venus`, `host/viewer`). I also used the docs listed in the brief, plus
  `docs/research/host-roundtrip-latency.md`, `guest/windows/docs/kmd-handoff-2026-10.md`, `zero-copy-present.md` §24 and
  `nvrm-forward-path.md`.
- **Live, read-only readings from the running VM (05:3x):**
  - two snapshots of the KMD service key, 4.1 s apart (desktop idle, DWM on NVK);
  - `conduit trace win11 --summary --duration 15`;
  - the backend command line and the conduit config (`conduit config get`);
  - the UMD `NVK present timing` and `present-gate` lines from today's runs, and `nvk-waits-14980.txt` (CS2 on 405.23).
- **Not run:** no GPU load, no latency capture under load, no StageTrace capture. Numbers marked *(doc)* come from earlier captures in the
  docs. Numbers marked *(live)* come from today's readings. Numbers marked *(est)* are reasoned estimates, with the reasoning given.

### State of this VM that matters for the numbers

| setting | value | effect |
|---|---|---|
| backend `--latency` | **off** on the measured setup (`backend.latency off`), conduit-venus `--no-direct-fences` | All four round-trip options are off: `quiet-held`, `fused-submit`, `direct-fences`, `event-batch`. This brings back about 10,000 event-thread MSIs/s under a game (vs about 1,400 with `event-batch`) and adds about 25 µs to every Venus fence return *(doc)* |
| `backend.cpus` | unset | Backend and venus threads float onto the vCPUs' cores *(doc: row D is −43 µs p50 per host round trip)* |
| KMD interrupts | **MSI-X** (`MsiMode=2`, `MsiModeEff=2`, `MsiVec=2`, `MsiGrant=3`) | Set by hand in this VM. The INF default is still INTx (`MSISupported=0`) |
| windowed copy | `RmCopyEngine=1` (KMD RM copy-engine channel, `CeChanUp=1`), `BltAsync=1`, `ForeignCopy=1`, `GuestBlob=0`, `CopyQueue=0`, `RedirVram=1` | The windowed copy can take the RM copy-engine route. The Venus ring-1 fallback stays on family 0 (graphics) |
| flips | `ForeignFlip=1`, `FfAsyncWin=2`, `IndepFlip=0`, `DirectFlipSupport` absent (0) | Every app is DWM-composed |
| GDI | `GdiAccel=1` | GDI jobs run on the HPD/GDI thread |
| UMD | `Icd=venus` but `DwmIcd=nvk` (DWM marker, so everything follows NVK) | Venus is still loaded as the fallback ICD |

## 1. Today's path, hop by hop, with cost per hop

### 1a. Per submission (vkQueueSubmit → GPU), all APIs

| # | hop | where | crossings | cost | vs bare metal |
|---|---|---|---|---|---|
| S1 | NVK writes GPFIFO entries, push, semaphore release + `NON_STALL_INTERRUPT` (+ WFI retire release, 0019) | `patches/0006 nvkmd_rm_exec_ctx_exec/flush`, `0009 emit_track_release`, `patches-common/0019` | none | ~µs CPU | Same as NVIDIA's user-mode submit. The WFI retire release per flush is extra (0019 workaround for the DXVK free bug) |
| S2 | GP_PUT to USERD, token to `usermode+0x90` | `0006` L375–395 | doorbell store through the shared window (one KVM memslot). Whether it exits is unmeasured | est 0–3 µs | Same as bare metal if it doesn't exit |
| S3 | Mesa submit thread (if a wait-before-signal pattern switched the queue to threaded mode) | `vk_queue.c:1066` | thread hop | 5–20 µs *(est)* | Upstream has this too. NVIDIA's driver does not |
| S4 | D3D12 only: HE12 Render record per ECL + KMD admission event | `umd12/src/forward12/nvk12.rs`, vkd3d 0008/0009 | 1 D3DKMT Render → KMD (no host), `SEMSURF_FENCE_CREATE` pre-made per batch (1 RM round trip, off the critical path) | commit→admission **1.85 ms avg** *(doc)*; dxgkrnl keeps ≤2 packets in flight (`QdExMax=2`) | Bare metal with HAGS: none of this, the submit goes straight to a HW queue |

**Verdict.** Submission is already at bare-metal shape for D3D11, Vulkan and GL: no syscall and no host hop per submit. D3D12 pays the
WDDM 2.1 software-scheduler packet pipe.

### 1b. Per GPU→CPU wait (fence, timeline, frame-latency waits in DXVK and vkd3d)

| # | hop | where | thread | cost (p50) |
|---|---|---|---|---|
| W1 | GPU semaphore release + nonstall IRQ on host CPU 0 | nvidia.ko | IRQ | – |
| W2 | RM OS-event fd readable → `nvgpu-events` epoll edge | `conduit-backend.rs:1384 event_pump`, `1188 EventReports::report` | backend event thread | 5–15 µs *(est)*; a fence already reached is found by the **1 ms sweep** |
| W3 | `EventReady` into one of **16** posted event buffers → `signal_used_queue` (one MSI per event with `event-batch` off) | `deliver_event` bin:615 | same | 1.4 µs irqfd. If all 16 are taken, the event waits for a repost (backlog) |
| W4 | guest ISR → DPC `drain_nvrm_events` → `KeSetEvent` (shared per-device nonstall event) | `kmd_render/src/virtio/gpu/nvrm_events.rs`, `ddi/interrupt.rs` | vCPU DPC | ~20–40 µs *(est from NvRtt min 18 µs)* |
| W5 | librmclient waiter wakes, wakes the other waiters via condvar (`crm_win_event_wait_gen`, TW:2346), each re-reads its semaphore | `transport_windows.c:2346`, `patches/0006 nvkmd_rm_wait_step` + `patches-common/0040/0042` | UMD threads | 10–30 µs *(est)*; **every release wakes every waiter** of the device |
| | **Total release → waiter running** | | | **~140 µs** *(doc: nvrm-forward-path.md "event round trip 142 µs")*. Bare metal (NVIDIA or nouveau): 5–15 µs |

Live frequency:
- CS2 (405.23, 384 fps): **≈2,800 event waits/s, mean 215 µs**, ≈600 ms/s of waiting summed over threads. That is about 7 waits per
  frame.
- Idle desktop: `NvEvSig` 585/s, MSIs 323/s, `NvEvFull` +6/s. The 16-buffer event ring overflows even at idle.

### 1c. Per present, D3D11 windowed (DWM-composed, legacy-blt model: Heaven windowed, CS2 windowed)

| # | hop | where | crossings | cost |
|---|---|---|---|---|
| P1 | `CopySubresourceRegion` into the DXGI destination | `umd/src/forward/present.rs nvk_present_impl_timed` L2089 | GPU blit | skipped (`NvkSkipBltCopy=1` default) |
| P2 | `context.Flush()` + **present gate, SUBMITTED mode**: the app thread blocks until DXVK's CS thread and submit thread have drained the frame | `present.rs:1631` → `dxvk_bridge.cpp:3037 present_frame_gate` → `d3d11_context_imm.cpp:1169 HeliosWaitFrameSubmitted` | 2 thread hand-offs | **CS2 278 µs (gate) / 340 µs (present-gate)**; DWM 133–148 µs; desktop apps 75–230 µs *(live)* |
| P3 | RM fence for the frame: `vk_queue_signal_sync` (extra GPFIFO kick) + `SEMSURF_FENCE_CREATE` escape | `patches-windows/0030 nvk_queue_rm_fence`, TW:1849 | 1 D3DKMTEscape → 1 host round trip | 60–80 µs *(doc)*; guest RM RTT median 40–60 µs, mean **168 µs** with a long tail (686 calls >1 ms, max 14.6 ms) *(live NvRtt)* |
| P4 | D3DKMT Present → KMD `DxgkDdiPresent` Blt arm → BltAsync / copy-engine route decision | `kmd_render/src/ddi/display.rs:196 → 263 → 961/1071` | none (async) | `PrDdiBlt` 8–16 µs *(live/doc)* |
| P5 | copy-engine route: GPFIFO + doorbell on the KMD's RM CE channel with a GPU wait on the producer's RM semaphore | `ddi/ce_present_route.rs`, `virtio/rm_client/ce_channel.rs:887 kick` | doorbell store | copy ≈ 5.76 MB / 28 GB/s ≈ **0.2 ms** on a copy engine, running beside graphics |
| P5' | (fallback) Venus ring-1 `SUBMIT_3D` on family 0, with the KMD worker deferring until the producer retires | `display.rs:2217 service_windowed_blt`, `virtio/venus/present.rs` | ctrl kick → backend → conduit-venus IPC → virglrenderer → `vkQueueSubmit` → fence back through 2–4 thread hops → MSI | deferral 0.61 ms + host round trip 0.40–0.45 ms p50 (0.75–0.83 p99) + guest submit/DPC ≈ 0.5–1 ms guest view *(doc)* |
| P6 | copy completion seen: **polled** (route settle spin up to 750 µs, then the HPD worker re-polls every 0.5 ms) | `ce_present_route.rs settle_after_dispatch`, `POLL_DUE_100NS` | none | 0–500 µs of quantisation per frame *(est)*. There is no completion interrupt (M3c not built) |
| P7 | DMA fence of the Present → `signal_dma_completed` (one per DPC, each with its own SyncExec and re-queued DPC) | `submit_command.rs:916`, `interrupt.rs:73 drain_used_and_complete` | MSI → ISR → DPC | 20–40 µs *(est)* |
| P8 | read-ledger retire → `broadcast()` = `KeSetEvent` on **every** registered event (AqLive=20 now, up to 64) at DISPATCH under `virtio_lock` | `adapter/read_ledger.rs:419, 685` | – | ≈1–3 µs per KeSetEvent: 20–200 µs of DPC time per retire, plus N thread wakes in other processes *(est)* |
| P9 | dxgkrnl: the next Present's Blt into the same redirection surface waits for the previous Blt packet to retire (`VIDMM_BEGINCPUACCESS_WAIT`) | dxgkrnl | – | was 1.0–1.7 ms on the Venus route *(doc)*. Should drop to about P5+P6+P7 on the CE route (not re-measured) |
| P10 | fence `Close` after it fired (second RM round trip, on the HPD worker) | `rm-fence-marker.md:403` | 1 host round trip | ~55 µs on a serial worker *(doc)* |
| P11 | DWM composes (its own NVK frame: present timing total ≈ **241 µs** per frame, gate 133 µs *(live, umd-1812)*), then ForeignFlip | `virtio/foreign_flip.rs` (async, `FfAsyncWin=2`) | RM `ScanoutFlip` kick; async ack | `FfRtt` mean **42.8 µs** *(live)*, submit 7.1 µs incl. 4.2 µs doorbell *(live SubF*)* |
| P12 | backend `handle_scanout_flip` → cached dma-buf → `sendmsg` to the viewer → `wl_surface_commit` → Hyprland composite (windowed) or direct scanout (fullscreen) | `nvidia/scanout.rs:26`, `display.rs:1893`, `host/viewer/nb_session_wl.c:1395` | 1 socket msg | a few µs CPU, **zero-copy**; +1 compositor frame when windowed in Hyprland (same as any Wayland client) |
| P13 | vsync to dxgkrnl: guest high-res timer at 240 Hz (SyncExec + DPC per tick), free-running vs the host vblank | `adapter/kobj.rs:849 service_vsync_tick` | – | 240 DPCs/s; beats against the real vblank (up to 1 frame of phase error) |

**Windowed budget.** Heaven windowed is about 2.1–2.4 ms per frame on 405.26+, against about 1.74 ms on bare metal (576 fps). The
**gap is ≈0.4–0.6 ms per frame**. CS2 windowed 1080p runs at 3.0 ms per frame (333 fps). Its present costs 432 µs on the app thread:
gate 278, finish 44, plus fence.

### 1d. Per present, fullscreen / scanout-0 path (Vulkan, D3D11 primary, D3D12 fenced scanout)

| # | hop | cost |
|---|---|---|
| F1 | present gate SUBMITTED (D3D11), or the ECL fence (D3D12) | as P2 |
| F2 | `SEMSURF_FENCE_CREATE` + `SCANOUT_PRESENT` with `RM_FENCE` (2 escapes) | 2 × 55–80 µs |
| F3 | KMD queues the flip, the worker sends `ScanoutFlip` when the fence's `EventReady` arrives | W-chain ≈140 µs + flip ≈43 µs |
| F4 | `vkAcquireNextImage`: `SCANOUT_STATUS` (KMD-only), wake on the host `ScanoutReleased` event; the KMD reposts without kicking and the host looks every 2 ms | 0–2 ms when the swapchain is short of images *(est)* |
| F5 | composed by DWM anyway, because IndepFlip is off: the app frame becomes a DWM input. FFXIV: 126 fps composed vs 247 on scanout 0 *(doc)* | a whole extra GPU composite and a DWM frame of latency |

### 1e. Per RM call (allocations, maps, fences, flips, KMD services)

Guest `escape_nvrm` → `nvrm_forward` → `ctrl.rs:630 raw_roundtrip`. That is 3 lock holds, a kick, a 50 µs spin, then KEVENT. It
continues to the backend `vring_worker` (single thread, one `Mutex<NvidiaBackend>` for all guest processes), one `ioctl`,
`signal_used_queue`, then MSI → ISR → DPC → `KeSetEvent`.

- Host share: **p50 21 µs, p99 56 µs** (`FB_GET_INFO_V2`, *live trace*).
- Guest-seen: **median 40–60 µs, mean 168 µs, 3.2% > 1 ms** (`NvRtt`, *live*). Native: 1.4 µs.
- Steady-state rate is low (34 controls/15 s idle), so this matters for present fences, flips, allocation bursts (level loads, CS2 map
  changes) and the deferred frees of 0019.

### 1f. Venus fallback path (processes on the deny-list, plus KMD paths still built on Venus)

App → Venus ICD (serialises Vulkan) → KMD ring → backend `Venus::dispatch` → IPC to conduit-venus → virglrenderer replays → host
NVIDIA Vulkan. Every fenced submit is a host round trip: 398–446 µs p50 including GPU *(doc)*. The host CPU share is about 105 µs
(50 µs in, 50 µs back). With `fused-submit` and `direct-fences` off (this VM) it is ~25–50 µs worse.

Still on Venus in the KMD:
- the ring-1 present-copy fallback;
- paging copies;
- the present ready-poll;
- the Venus scanout path when ForeignFlip is off;
- GuestBlob.

### 1g. Background guest costs (no frame on screen)

| item | evidence | cost |
|---|---|---|
| registry mirror thread | `HpdDumpUs` 23.66 s over 274 dumps = **86 ms per full dump**, `MirLastUs` 160–272 ms, 2,571 values *(live)* | about 2–4% of one vCPU, at priority 6. Pre-empts game threads on a 16-vCPU guest only rarely, but it is pure overhead |
| vsync timer DPCs | `VsTickN` +240/s, `DpcNoCause` +250/s *(live)* | about 240 DPC + SyncExec per second |
| event relay | 585 KeSetEvent/s and 323 MSIs/s at idle *(live)*; host trace: 485 `event` requests/s idle | small, but the 16-entry ring overflows (`NvEvFull` +6/s) |
| GDI accel | `GdiUs`/`GdiCmdN` = **1.8 ms per GDI job** mean, 44 ms max; 1,773 CPU fallbacks *(live)* | blocks the HPD worker / GDI thread; desktop smoothness, not games |

## 2. Gaps vs bare metal, ranked by expected gain

Bare-metal reference behaviour:
- NVIDIA on Windows has HAGS hardware queues, monitored fences written by the GPU and seen through MSI-X, flip queues of 2–3, MPO and
  independent flip for full-screen flip-model swapchains, and real vblank interrupts.
- On Linux, NVIDIA's userspace (as in a Conduit Linux guest) submits through USERD/doorbell, waits through per-semaphore
  `NV_SEMAPHORE_SURFACE` waiters (one wake per waiter, in-kernel), and presents through KMS with plane fences.
- Upstream NVK makes one `NOUVEAU_EXEC` per submit and waits per syncobj in the kernel.

| rank | gap | where we pay it | est. gain | conf. |
|---|---|---|---|---|
| G1 | **Composition of full-screen and flip-model apps** (IndepFlip/DirectFlip off, no MPO) | every game frame becomes a DWM input: extra composite, extra DWM frame, the windowed copy | full-screen games: up to ~2× in the composed-bound case (FFXIV 126 → 247), plus one frame less latency | high for the cost, medium for the fix (the iflip freeze needed HwCursor=1) |
| G2 | **Present gate (SUBMITTED) blocks the app thread** on DXVK's CS+submit threads every present | P2/F1: 150–340 µs per frame on the app's render thread | CPU-bound titles: 5–11% frame time (CS2: 278 µs of 3.0 ms) | high (live numbers) |
| G3 | **GPU→CPU wake chain is ~140 µs with herd wakeups**, 16-buffer event ring, `event-batch` off, 1 ms sweep | §1b: ~7 waits per frame in CS2; DXVK frame pacing and vkd3d fence waits sit on this chain | 50–100 µs off each wait. On the critical path for 1–2 waits per frame, so 0.1–0.2 ms per frame | medium |
| G4 | **Windowed copy completion is polled** (0.5 ms worker poll, 750 µs settle spin), and the Venus fallback still defers on the CPU | P6, P9: dxgkrnl serialises the next Blt behind it | 0.1–0.4 ms per windowed frame | medium (needs StageTrace on the CE route) |
| G5 | **Two RM round trips per present for the fence** (create + close), on a serial worker | P3/P10: 60–80 µs on the app thread + 55 µs on the HPD worker | 60–80 µs per frame on the app thread; less head-of-line blocking on flips | high |
| G6 | **D3D12 packet pipe** (no HW queues, WDDM 2.1, `QdExMax=2`, admission 1.85 ms) | S4 | D3D12 titles: large; presenting thread off-CPU 11 ms of a 20.6 ms frame *(doc)* | medium |
| G7 | **Host latency options off and no CPU placement** in this VM | §1b, §1f | −25 µs on every Venus fence return, −8,600 MSIs/s, −43 µs per host round trip with `backend.cpus 0-3,16-19` | high (measured A/B in research doc) |
| G8 | **RM call RTT 40× native**: single backend worker + mutex, 3 lock holds + spin in the KMD, shared 64-descriptor ctrl queue without EVENT_IDX | §1e | level loads and stutters (the >1 ms tail: 3.2% of calls); per frame only via G5 | medium |
| G9 | **Synthetic vsync** not locked to the host vblank; flip queue depth 1 | P13, flip caps | judder and latency (up to 1 frame), not throughput | medium |
| G10 | **Read-ledger broadcast** to every registered event | P8 | 20–200 µs DPC per retire + N wakes; grows with the number of D3D11 processes | high that it costs, low that it is large today (AqLive=20) |
| G11 | **Venus still required** (`--venus`): fallback ICD, KMD ring-1 fallback, paging, GuestBlob | §1f | removes a process, a host Vulkan device and ~1 GB of host state; perf only for denied apps | high for simplification |
| G12 | Background guest overhead: 86 ms registry dumps, 240 vsync DPCs/s, GDI jobs 1.8 ms | §1g | 2–4% of a vCPU; desktop smoothness | high |

## 3. Prioritised TODO list

Columns: what to change | why | expected gain (confidence) | effort | risk | how to measure.
Tags: **[V]** removes a Venus dependency, **[F]** enables flip or independent flip.

### T1. Re-enable the host latency options and pin the backend (config only)
- **What.** Restore `backend.latency` to its default (`conduit config unset backend.latency`, i.e. `all`) and set
  `backend.cpus 0-3,16-19`, after bisecting which of host399–402 caused the cursor stutter. The bisect is per option:
  `quiet-held`, `fused-submit`, `direct-fences`, `event-batch`. host404 already fixed the event re-report storm, so the cursor issue may
  no longer reproduce.
- **Why.** This VM runs with all four off. That means ≈10,000 event MSIs/s under load, +25 µs per Venus fence return, and +30 µs of
  queue-thread time per fenced submit.
- **Gain.** −40 µs p50 per host round trip, −85% event-thread interrupts (high: measured rows A–D in
  `docs/research/host-roundtrip-latency.md`).
- **Effort / risk.** S. Low; the cursor regression must be re-checked by hand.
- **Measure.** `conduit trace win11 latency` (rows 16–18, interrupts/s); KMD `MsiInts` rate; the user's cursor test.

### T2. Make the D3D11 present non-blocking: drop the SUBMITTED gate on the app thread
- **What.** `umd/src/forward/present.rs:1631` and `dxvk_bridge.cpp:3037 present_frame_gate` → DXVK `HeliosWaitFrameSubmitted`.
  - Instead of waiting for DXVK's CS and submit threads, reserve the frame's RM fence value at record time on the CS thread
    (`HeliosSignalPresentFence`-style).
  - Hand the KMD a (fence, value) pair it waits on GPU-side (copy-engine route) or worker-side (flip).
  - Let `D3DKMT Present` go out immediately. This is the same split DXVK does natively: `presentImage` is queued to the submit thread.
- **Why.** The present gate is 150–340 µs per frame on the app's render thread (CS2 278 µs, DWM 133 µs). Bare metal returns from
  Present after queueing.
- **Gain.** 5–11% frame time for CPU-bound D3D11 titles; DWM −130 µs per composed frame (high on the size, medium on how much becomes
  fps).
- **Effort / risk.** M. Medium: ordering with the KMD Blt (the Heaven "jump" bug from 404 was exactly an ordering bug); the read-ledger
  and present-stream correlation must carry the reserved value.
- **Measure.** `NVK present timing gate=` → ~0; `present-gate avg_us`; PresentMon `MsInPresentAPI` (`HELIOS_VK_FRAMETIME`); StageTrace
  stage 1→3.

### T3. Per-waiter GPU wakes instead of one shared nonstall event per device
- **What.**
  1. Give each NVK sync (or each waiting thread) its own wake source: an RM `NV_SEMAPHORE_SURFACE` waiter
     (`NV_SEMAPHORE_SURFACE_CTRL_CMD_REGISTER_WAITER` with a notification) mapped to a per-waiter KMD event. The KMD signals only the
     waiter whose value was reached.
  2. Replace `crm_win_event_wait_gen`'s wake-all condvar (TW:2346) and the 1 ms poll stop-gap (`patches-common/0040`).
  3. Grow the KMD event ring from 16 to 128/256 buffers (`nvrm_events.rs:91 EVENT_QUEUE_SIZE`; the comment says the boot-stack budget
     must be measured first: heap-allocate the queue state).
  4. Have the backend sweep the semaphore value on fd edge rather than every 1 ms.
- **Why.** About 140 µs per wake vs 5–15 µs native. Every release wakes every waiter. `NvEvFull` overflows even at idle. CS2 makes
  ≈2,800 waits/s.
- **Gain.** 50–100 µs per wait. 0.1–0.2 ms per frame where DXVK/vkd3d frame-latency waits are on the critical path (medium). Removes
  most 1 ms-poll timeouts (high).
- **Effort / risk.** L across NVK + rmclient + KMD + backend. Medium risk: the host semaphore-surface waiter path exists for Linux
  guests (`docs/SYNC.md`, `fence.rs`), so reuse it.
- **Measure.** `NVK_WAIT_STATS=1` (mean wait, timeouts); `NvEvFull`/`NvEvMaxP`; MSIs/s; host `conduit trace latency`.

### T4. Event-driven completion for the KMD copy-engine route; retire the Venus ring-1 fallback [V]
- **What.**
  - `ddi/ce_present_route.rs` and `virtio/rm_client/ce_channel.rs`: add the M3c completion. The CE push ends with a semaphore release +
    `NON_STALL_INTERRUPT` on an RM event the KMD registered, so the `EventReady` → DPC completes the WDDM fence directly. That replaces
    the 750 µs settle spin and the 0.5 ms worker poll.
  - Also fix `ce_vram::wait`/`wait_value`'s `sleep_ms(1)` (15.6 ms real) → `KeDelayExecutionThread` with a high-resolution timer, or
    the event.
  - Once the route is the default and clean, drop the Venus ring-1 Blt fallback, the producer deferral (`service_windowed_blt`) and
    GuestBlob.
- **Why.** The windowed copy is 0.2 ms of copy-engine time, but completion is quantised by a 0.5 ms poll. dxgkrnl serialises the next
  Blt behind it (P9).
- **Gain.** 0.1–0.4 ms per windowed frame (medium; confirm with StageTrace on the CE route first). Removes the Venus present dependency
  (high).
- **Effort / risk.** M. Medium: the WDDM fence-ordering invariants (`zero-copy-present.md` 24.4).
- **Measure.** `StageTrace=1` + `guest/windows/ci/vmtest/stages.sh` (present → done); `CeRtDoneUs`, `CeRtPollUs`; ETW 41→42 on the app
  thread.

### T5. One RM round trip (or none) per present fence
- **What.**
  - **Guest side.** Pre-create a ring of present fences per swapchain or device (as vkd3d 0009 already does for ECL fences:
    `helios_vkd3d_prepare_ecl_fence`) and reuse them by value on one semaphore surface. That removes `SEMSURF_FENCE_CREATE` from
    `patches-windows/0030 nvk_queue_rm_fence` and the `Close` from the KMD worker.
  - **Host side (alternative).** Batch create+wait into one message, and close on signal in the backend without a guest message.
- **Why.** 60–80 µs on the app thread plus ~55 µs on the serial HPD worker per present. The README notes a trivial frame is slower
  this way (5645 → 4899 fps).
- **Gain.** 60–80 µs per frame on the app thread; less flip head-of-line blocking (high).
- **Effort / risk.** S–M. Low–medium: fence value reuse vs KMD marker bookkeeping (`rm-fence-marker.md`).
- **Measure.** host `conduit trace --summary` (drm call count per frame → 0); `NvRttN` rate; `NVK present timing finish=`/`total=`.

### T6. Independent flip / DirectFlip on by default for full-screen flip-model apps [F]
- **What.**
  - Finish the iflip root cause from 405.7: the CS2 fullscreen freeze. It needed `HwCursor=1`, and 405.10+ makes HwCursor follow
    IndepFlip.
  - Then default `IndepFlip=1` and `DirectFlipSupport=1` (`query_adapter_info.rs:470–485`, `umd/src/forward/transfer.rs:369`
    CheckDirectFlipSupport, D3D12 UMD slot).
  - Raise `MaxQueuedFlipOnVSync` to 2 with `FlipDoneHost` (`FlipQueueN`).
  - Grow `SCANOUT_ALLOCS` beyond 32.
- **Why.** Every game is a DWM input today. On bare metal a full-screen flip-model swapchain goes to independent flip, so no composite
  and no copy.
- **Gain.** Full-screen games: removes the DWM composite and up to one frame of latency; FFXIV-class 126 → ~247 fps (high on cost,
  medium on stability).
- **Effort / risk.** M. Medium–high: the freeze and the focus loss; PR #31 was closed for exactly this.
- **Measure.** PresentMon `PresentMode` = "Hardware: Independent Flip"; `IfN`/`IfGap*`, `FfFrames`; DWM `NVK present timing` drops to 0
  while the game is up.

### T7. Read-ledger: signal only the owner, after dropping the locks
- **What.** `adapter/read_ledger.rs:685 broadcast`. Record the waiting registration per slot (resid → owner event). Collect the event
  references under the table lock and call `KeSetEvent` after releasing `virtio_lock`, or queue a DPC. Also fix the dead
  `event_registered=true` after PnP stop/start (10 ms timeout path).
- **Why.** Up to 64 `KeSetEvent` per retire at DISPATCH under `virtio_lock`, which wakes every DXVK signaler in every process
  (`AqLive` = 20 now).
- **Gain.** 20–200 µs of DPC per retire, fewer spurious thread wakes; scales with the number of open D3D11 apps (high that it helps,
  modest size).
- **Effort / risk.** S. Low.
- **Measure.** `AqSigB` per retire → ~1; DPC time (xperf DPC/ISR); `virtio_lock` hold (`SubLock`).

### T8. D3D12: get off the 2-packet software queue
- **What.**
  - Report WDDM 2.x hardware queues (`CreateHwQueue`/`SubmitCommandToHwQueue`, `scheduler.rs:182/200/240` currently
    `STATUS_NOT_SUPPORTED`) with monitored fences backed by RM semaphores written by the GPU. The KMD reports the monitored-fence
    address; dxgkrnl reads the GPU-written value.
  - Interim: deeper `HwQueuePktCap` is ignored at 2.1, so raise the reported WDDM version or use a second node (`D3d12Node`, tried:
    no gain).
- **Why.** Admission 1.85 ms; presenting thread off-CPU 11 ms of a 20.6 ms frame; `QdExMax=2`.
- **Gain.** Large for D3D12 titles (medium; the 2-slot pipe was not fully proven as the cause: `Umd12MergeEcl` diagnostic).
- **Effort / risk.** L. High: a new WDDM model in the KMD.
- **Measure.** `umd12-*.log` frame time split (ECL CPU wait, commit→admission); ETW DxgKrnl queue packets.

### T9. Make MSI-X the INF default, with the breaker
- **What.**
  - INF requests message-signalled interrupts. Keep `MsiBreaker` (present) as the fallback to INTx.
  - Collapse the 7–8 lock holds in `interrupt.rs:73 drain_used_and_complete` into one pass.
  - Signal all ready WDDM fences in one `DxgkCbNotifyInterrupt` batch (`take_one_ready_wddm` → loop) instead of one
    SyncExec + DPC requeue per fence.
- **Why.** MSI-X runs in this VM only because it was set by hand. INTx adds a status-register exit and EOI, plus QEMU main-loop tails
  up to 1.6 ms *(doc)*.
- **Gain.** Tens of µs per completion; removes INTx ms tails for every user (high).
- **Effort / risk.** S for the INF and default, M for the DPC restructuring. Medium: a wrong INF leaves the device without interrupts,
  which the breaker covers.
- **Measure.** `MsiModeEff`; `NvRtt` histogram; `BltAsyncLat`/`CeRtDoneUs`; xperf DPC time.

### T10. Shorten the RM call path (backend and KMD)
- **What.**
  - **Backend.** Split `vring_worker` dispatch so RM calls of different clients do not serialise on one `Mutex<NvidiaBackend>`
    (per-client or read-mostly locking; a worker pool for long calls). Turn on `EVENT_IDX` on the KMD side (`gpu/mod.rs:2998`,
    `event_idx=false`) to cut kicks and interrupts.
  - **KMD.** Avoid 3 lock holds per enqueue. Make the 50 µs pre-wait spin adaptive (it is, `NvSpinUs`; hit rate 89%). Add a second
    ctrl queue for RM traffic so flips and copies don't queue behind slow RM calls (head-of-line: 686 calls > 1 ms).
- **Why.** Guest-seen mean 168 µs vs host 21 µs. The >1 ms tail stalls flips and the HPD worker.
- **Gain.** Mean RTT → ~40 µs; fewer stutters on allocation bursts (medium).
- **Effort / risk.** M–L. Medium: locking in the backend.
- **Measure.** `NvRttB*`/`NvRttMeanUs`; host trace `queue_us` vs `host_us`.

### T11. Host-paced vsync (`HostVblank`) and flip-done from the host [F]
- **What.** Build `guest/windows/docs/host-vblank-pacing.md` (design only today): the backend forwards the viewer's
  `wp_presentation` feedback as a vblank stamp, and `service_vsync_tick` locks phase to it. Use `FlipDoneHost=1` so flips retire on the
  real present.
- **Why.** The guest's 240 Hz timer beats against Hyprland's vblank: up to one frame of phase error, judder at 240 Hz.
- **Gain.** Latency and pacing, not throughput (medium).
- **Effort / risk.** M. Medium.
- **Measure.** `VsLate*`, `FlipHostLat*`, `FlipLat*`, PresentMon display latency.

### T12. Remove the HPD worker's head-of-line blocking
- **What.** Move flip programming and ForeignFlip off the single PASSIVE HPD worker (`ddi/hpd.rs`). It currently also runs Blt
  dispatch, CE settle, RM fence closes, the RM client and GDI. Give flips a dedicated high-priority thread (or do them from the DPC with
  a pre-built message). Keep GDI on `gdi_thread.rs` only.
- **Why.** Flips wait behind 55 µs RM closes, 1.8 ms GDI jobs (44 ms max) and CE polls.
- **Gain.** Flip-latency tails; desktop smoothness (medium).
- **Effort / risk.** M. Medium: lock order scanout → venus → virtio.
- **Measure.** `HpdBusyUs`, `HpdPassMaxUs`, `FlipPrgLat*`, `FfRtt*`.

### T13. Cut background guest overhead
- **What.** The registry mirror writes 2,571 values at 86 ms per full dump. Write only changed values, use a REG_BINARY block per
  subsystem instead of thousands of REG_DWORDs, and stretch the full rewrite from 30 s to on-demand (an escape that dumps on request
  for tooling). Keep `StageTrace` as the per-frame source.
- **Why.** 2–4% of a vCPU for telemetry.
- **Gain.** Small but free (high).
- **Effort / risk.** S–M. Low (tooling reads the names: keep a compatibility mode).
- **Measure.** `HpdDumpUs`/`MirLastUs`.

### T14. Venus removal (S6d) [V]
- **What.** After T4, follow the `docs/HANDOFF.md` "Removing Venus" order:
  1. KMD waits on RM fences (paging, present ready-poll);
  2. shrink the deny-list to DXR;
  3. remove the Venus scanout path (ForeignFlip only);
  4. drop `--venus`, conduit-venus, the virglrenderer patches and `--venus-guest-blobs`.
  Interim: `conduit up` should enable Venus automatically for Windows guests.
- **Why.** A second Vulkan stack on the host, extra process hops, and GuestBlob complexity.
- **Gain.** Simplification and host memory; perf only for denied apps (high on simplicity).
- **Effort / risk.** L. Medium.
- **Measure.** `CqMain`/`RvVenus`/Venus dispatch count → 0 in `conduit trace`.

### T15. GDI path
- **What.** GDI jobs average 1.8 ms (`GdiUs/GdiCmdN`), with 1,773 CPU fallbacks. Extend copy-engine coverage (fix/text-glyphs adds a CPU
  fallback for failed CE copies to NVK images; the reverse, more CE coverage, is the speed lever). Batch the jobs of one RenderKm.
- **Gain.** Desktop/Explorer smoothness (medium). **Effort** M. **Risk** medium (the text-rendering correctness work is in flight;
  coordinate). **Measure:** `GdiUs`, `GdiFall`, `GdiCeSub`.

Smaller items:
- **`CopyQueue=1` [V-interim].** Puts the Venus ring-1 fallback copy on the transfer family: 2.3 ms → 0.3 ms under graphics load
  *(doc)*. Implemented but hardware-unverified. Only matters while the Venus fallback is used, so it is a cheap A/B row. S, low risk.
- **Doorbell exit.** Measure whether the USERD/usermode doorbell store exits (it lands in the BAR-backed window). If it exits, check
  KVM's handling of that slot. `SubFKick` = 4.2 µs per virtio kick *(live)* is the comparison point.
- **`patches-common/0019` WFI retire release.** One WFI per flush. Once the DXVK free bug is found (0021 thread names), make the
  deferral cheaper: a release without WFI on a separate semaphore.
- **Mesa threaded-submit switch.** Advertise WAIT_BEFORE_SIGNAL handling so vkd3d does not push the queue into permanent submit-thread
  mode.

## 4. Quick wins vs big rewrites

**Quick wins (days, config or small code):**
1. T1: latency options back on + `backend.cpus` (config, after the cursor check).
2. T7: read-ledger owner-only signal (S).
3. T5: pre-made present fences, no create/close per frame (S–M, the vkd3d pattern exists).
4. T9 (INF part): MSI-X default with the breaker (S).
5. `CopyQueue=1` A/B for the Venus fallback copy (knob only).
6. T3 step 3: event ring 16 → 128+ buffers (S, after the stack-budget fix).
7. T13: registry mirror diet (S–M).

**Medium rewrites (weeks):**
- T2: non-blocking D3D11 present: the largest per-frame CPU item.
- T4: event-driven CE completion and dropping the Venus present fallback [V].
- T6: independent flip by default [F]: the largest full-screen item.
- T11: host vblank [F].
- T12: HPD worker split.
- T10: RM path / backend locking.

**Big rewrites (months):**
- T3: per-waiter GPU wakes (NVK + rmclient + KMD + backend).
- T8: WDDM hardware queues and monitored fences for D3D12.
- T14: Venus removal [V].

## 5. What to measure first (before writing code)

1. **StageTrace on windowed Heaven, CE route** (`StageTrace=1`, `stages.sh win11 10`). This is the per-stage table for P2–P9 on the
   current route. Kind-1 stamps exist for the Venus path. Check that the CE route stamps stages 3–5 (if not, add them: that is part of
   T4).
2. **`NVK_WAIT_STATS=1` + `HELIOS_VK_FRAMETIME=1` on CS2 at 1080p**, with T1 on and off. This splits frame time into present gate,
   waits and GPU.
3. **`conduit trace win11 latency` for 5 s under the same load**, to get the event-thread MSI rate and the Venus round trip with
   options on and off.
4. **xperf DPC/ISR on the guest** for T7/T9: DPC time per retire, per vsync tick, per completion.

Do these on the lowest mode (1920x1080@240) and keep captures under 30 s.
