# D3D11 threaded front end (UMD worker thread)

Status: phase 1, off by default; `UmdThreadedFrontend=1` turns it on for every process except dwm.exe.
`UmdFrontendSync` (section 7) takes one call class at a time off the threaded path for A/B runs.

## 1. Why

CS2 on the win11 guest (PERF-ROADMAP.md §1h) is bound by one thread: `D3D11RenderThread` runs at 99.7% of a core and
sets the frame rate. With the D3D11 front end inline on that thread (front end off, ~150 fps, 6.6 ms per frame), per
frame:

| where | ms | share |
|---|---|---|
| `helios_umd.dll`: our DDI forward layer + DXVK's D3D11 front end | 3.0 | 46% |
| `d3d11.dll` (Microsoft runtime above the UMD) | 1.4 | 21% |
| game code | 1.4 | 21% |
| waits and wakes on DXVK threads | 0.4 | 6% |

The UMD share is the D3D11 front end running on the app's thread: per call it resolves handles, updates the DXVK
binding state, builds CS commands and allocates buffer slices (`Map(WRITE_DISCARD)` alone is about 17% of the
thread). DXVK already moves the Vulkan work to its CS thread; what stays on the app thread is the front end before
it. On bare metal NVIDIA's "threaded optimization" moves the equivalent driver work to a driver thread, and the app
thread pays little beyond the runtime.

The threaded front end does the same in our UMD: the immediate-context DDI calls the app makes are recorded into a
ring as small closures and a UMD worker thread replays them into the forward layer and DXVK. Only calls that have to
answer synchronously stay on the app thread.

The aim is a UMD share of 5–10% on the render thread and a frame of about 4 ms, as long as the worker (≈3 ms of
front-end work per frame) and `dxvk-cs` keep up.

## 2. Where to defer: the DDI boundary

Three places could take the cut:

1. **In DXVK** (make `D3D11ImmediateContext` record to a worker). DXVK's front-end code is the cost, so this would
   leave our forward layer on the app thread and needs invasive changes to a fork we rebase.
2. **At the COM boundary** (between the forward layer and DXVK). Leaves the forward layer (handle resolution,
   `bind_input_layout`, the lazy input-layout cache) on the app thread.
3. **At the DDI boundary**, before the forward layer. Both the forward layer and DXVK's front end move to the worker.
   The forward layer's handle encoding already supports it (below).

We take 3. The D3D11 runtime calls our device-funcs table; for the immediate context, every slot that can be deferred
is replaced by a recording wrapper. The wrapper captures its arguments by value and pushes a closure that later calls
the unchanged forward function (`draw_indexed`, `set_constant_buffers1_common`, …) on the worker.

### Handles: identity words and pseudo-handles

Every DDI object handle region this UMD hands the runtime holds one identity word at offset 0: a COM pointer
(shaders, views, state objects, queries, command lists) or a `Box` pointer (`ResourceState`, `RtvState`,
`LayoutData`). Deferred-context handle regions already work by copying that one word (`deferred.rs`), and every
forward read path (`load_com`, `resource_state`, the bind collectors) reads only `*pDrvPrivate`.

A recording wrapper therefore reads the identity word of each handle argument at record time and stores the word in
the record. On replay it passes a pseudo-handle whose `pDrvPrivate` points at the stored word. The runtime may free or
reuse the handle region the moment the app's call returns; the record no longer depends on it. A null handle is
recorded as a reserved odd value and replayed as a null `pDrvPrivate`, so "unbind" and "empty slot" keep their
current meanings.

The two exceptions are already safe: query regions carry a second word (the DDI query kind) that only `GetData`
reads, and `GetData` runs on the app thread; and a handful of resource paths key on the region address
(`nvk_keyed_resources`, `gdi_handoff`), which are synchronous (shared resources, DXGI).

## 3. Structure

One `Frontend` per immediate context (per `HeliosDevice`), created at `CreateDevice` when the knob enables it; the
worker thread starts on the first recorded call (most D3D11 devices in a process never draw: DXGI probes, DWM
helper devices).

- **Ring**: a fixed byte ring (default 4 MiB, `UmdFrontendRingKiB`), single producer / single consumer. Producer and
  consumer cursors are monotonic `u64` byte counts on separate cache lines. A record is
  `{ call: fn pointer, len, extra_off, extra_len }` + the closure's bytes + trailing data (handle words, small
  payloads), 8-byte aligned, never straddling the end (a pad record fills the tail). Closures must be at most 8-byte
  aligned (checked at compile time) and own everything they need.
- **Producer**: whichever thread is inside an immediate-context DDI. The runtime serialises immediate-context calls
  (the D3D11 device lock, or the app's promise under `D3D11_CREATE_DEVICE_SINGLETHREADED`), so there is one producer
  at a time; the producer state is not thread-local, and the runtime's lock gives the happens-before between
  successive producer threads.
- **Worker**: pops records in order and calls them, then advances the consumer cursor (`done`). It spins 100 µs on
  an empty ring, then parks (1 ms, then 4 ms, then 50 ms after a quarter second idle); the producer unparks it only
  when it published into a ring the worker announced it is sleeping on (Dekker-style flag + recheck: the producer's
  cursor store is SeqCst, so it is ordered before its read of the flag), so a busy frame costs no wakes.
- **Waiting for the worker**: `wait_done(pos)` returns once `done >= pos`; `drain()` is `wait_done(write cursor)`.
  Spins briefly, then sleeps on a condvar the worker signals after each batch while anyone waits.
- **Backpressure**: a producer that finds no room waits for the worker (spin, then the same condvar). Memory is bounded
  by the ring; a record larger than a quarter of the ring is never built (the call takes the synchronous path).
- **Thread identity**: `is_worker()` is a thread-local read + compare.
- **Process exit**: if `DestroyDevice` runs after process termination has killed the worker
  (`RtlDllShutdownInProgress`), waits return and shutdown neither joins nor replays.

A device gets a front end only when the knob says so, FREETHREADED caps are on (with caps 0 the runtime serialises
`Create*`/`Destroy*` with the immediate context; a worker would quietly undo that kill switch), and the app did not
create it with `D3D11_CREATE_DEVICE_PREVENT_INTERNAL_THREADING_OPTIMIZATIONS`
(`D3D10DDI_CREATEDEVICE_FLAG_DISABLE_EXTRA_THREAD_CREATION`).

## 4. Call classes

Every immediate-context entry point is in exactly one class.

| class | what happens on the app thread | examples |
|---|---|---|
| **deferred** | snapshot args, push a closure, return | `*SetShader`, `*SetConstantBuffers{,1}`, `*SetShaderResources`, `*SetSamplers`, IA binds, `SetRenderTargets`, viewports/scissors, blend/depth/raster state, `CsSetUnorderedAccessViews`, `SoSetTargets`, `SetPredication`, all draws and dispatches, clears, `Discard`, `ClearView`, copies, resolve, `GenMips`, `CopyStructureCount`, `QueryBegin/End`, buffer `UpdateSubresourceUP`, `Flush` (no shared resources), `CommandListExecute` (phase 2; synchronous in phase 1) |
| **split** | answer now, defer the rest | `Map(WRITE_DISCARD/NO_OVERWRITE)` of a dynamic buffer and its `Unmap`, `QueryGetData`, object destruction |
| **synchronous** | `drain()`, then the unchanged forward call on the app thread | `Map` of everything else (staging, `READ`, `WRITE`, textures), texture `UpdateSubresourceUP`, the DXGI DDIs that order against rendering (`Present`, `Present1`, MPO presents, `Blt`, `Blt1`, `RotateResourceIdentities`, `SetDisplayMode`, `ResolveSharedResource`, offer/reclaim), `Flush` with shared resources, `Discard` of a buffer, `AcquireResource/ReleaseResource`, `CommandListExecute` (phase 1), resource destruction (waits for the worker, then runs on the calling thread); tiled-resource DDIs, markers and video DDIs through the safety net |
| **device-level** | unchanged, inline, concurrent with the worker | `Create*`, `Open*`, `Calc*`, `Check*`, `RetrieveSubObject`, `RelocateDeviceFuncs`, the free-threaded `RecycleDestroyCommandList` / `RecycleCreate*`, the caps-only DXGI DDIs |

"Synchronous" means the ring is drained, not that the worker is idle: it may still be running queued destructions,
whose bodies are free-threaded safe (cache mutex, atomics). A resource destroy runs the destructions the worker has
passed on its own thread first, and waits for any the worker already took out of the queue and is still running
(`CmdQueue::run_ready_retired` waits on counters of destructions taken and finished; the worker itself never waits
for another thread's destruction), so a view is never destroyed after its resource.

Device-level calls already run concurrently with the immediate context under `UmdFreeThreaded` (FREETHREADED caps),
so their thread-safety is not new. Moving immediate-context work from the app thread to the worker does not change
which data is shared between threads, only which thread does the immediate-context side.

**Safety net**: `d3d11_context(h)` (the single accessor every forward function uses to reach the DXVK immediate
context) drains the frontend first when it is called on a thread other than the worker. Any immediate-context path
that was not classified, or that a future change adds, degrades to synchronous instead of racing the worker. The
deliberate app-thread exceptions (`GetData`, the split map) do not go through it.

Its limits: a drain on a thread that is not the producer does not keep the producer from queueing more afterwards,
so it only orders the producer thread's own paths; and bridge entry points that use DXVK's immediate context
directly (`present_frame_gate`, `flush_gate_point`, `handoff_publish`, `rotate_resource_backings`,
`dxgi_blt_convert`, `publish_present_order`, the present copy/snapshot/vehicle calls, `wait_submitted`) bypass it.
All of those sit on paths whose wrappers drain explicitly (DXGI, the shared-resource flush), which is the actual
guarantee; the net is the backstop for the rest.

**Runtime callbacks never run on the worker.** `pfnRenderCb`, `pfnPresentCb`, `pfnAllocateCb/DeallocateCb`,
`pfnLockCb`, residency callbacks and `pfnEscapeCb` are only called from inside a DDI call on the calling thread; every
path that makes one is synchronous (present, flush gate with shared resources, resource destruction, creation). We do
not use `pfnSetAsyncCallbacksCb`. The one callback a deferred forward function can reach is `pfnSetErrorCb` (§5.10);
on the worker it is replaced by a stash.

## 5. Semantics

### 5.1 Immediate vs deferred contexts, command lists

Only the immediate context is threaded. Deferred contexts (DCs) already record on the app's own threads into DXVK's
`D3D11DeferredContext` through their own funcs tables (`deferred.rs`); those tables are filled independently of the
device table and get no wrappers. A DC's calls never reach the frontend's ring.

`FinishCommandList(FALSE)` clears the DC's DXVK state, so it forgets what the DC last handed DXVK
(`CtxBindings::forget_dxvk_bindings`: `bound_vs_com`, `bound_il_com`) and keeps the API-level shadows: the draw path
skips `IASetInputLayout` when `bound_il_com` names the layout DXVK already has, and a stale value would leave the next
list's draws without one.

`CommandListExecute` on the immediate context is deferred from phase 2 on (phase 1 drains and runs it inline, so no
execute is ever queued when a list is recycled). The record holds its own reference to the command list
(AddRef at record time, Release after replay), and the frontend remembers the ring position of the latest execute.
The BUILD_2 recycle path (`RecycleCommandList` on a DC hands an escrowed list to DXVK's cache, which resets it) first
waits until the worker has passed that position, so a list is never reset while an execute of it is still queued.
`RecycleDestroyCommandList` / `RecycleCreateCommandList` / `RecycleCreateDeferredContext` are free-threaded and stay
device-level. `DestroyCommandList` goes through the destruction path (§5.5). `QueryEnd`s inside an executed list do
not stamp `end_pos`; DXVK's own query reset counter covers that; phase 2 has to recheck it.

### 5.2 Map / Unmap

The runtime calls specialised slots, which tell us the case:

| slot | route |
|---|---|
| `DynamicConstantBufferMapDiscard`, `DynamicIABufferMapDiscard`, `DynamicResourceMapDiscard` on a dynamic buffer | split: discard |
| `DynamicIABufferMapNoOverwrite`, `DynamicConstantBufferMapNoOverwrite` on a dynamic buffer | split: no-overwrite |
| any of the above on anything else (dynamic texture), `ResourceMap`, `StagingResourceMap` | synchronous |
| `Dynamic*Unmap` of a dynamic buffer | nothing (DXVK's buffer `Unmap` is a no-op) |
| other `Unmap`s | synchronous |

"Dynamic buffer" is a flag cached in `ResourceState` at creation (`D3D11_USAGE_DYNAMIC` buffer).

**Discard (split).** DXVK's `MapBuffer(WRITE_DISCARD)` is two halves: `D3D11Buffer::DiscardSlice` allocates a new
backing slice and makes it the buffer's current mapped slice (`m_mapPtr`), then `EmitCs(invalidateBuffer)` queues the
rename for the CS thread (plus `ThrottleDiscard` for large buffers). A DXVK patch (§6) exposes them separately:

- app thread: `HeliosFrontendDiscard(buffer, &ptr)` runs `DiscardSlice` with a second `DxvkLocalAllocationCache`
  owned by the app-thread side (the context's cache is the worker's), returns the slice and its mapped pointer. The
  pointer goes back to the runtime at once.
- the record: a closure owning the slice reference; on the worker it calls `HeliosFrontendInvalidate(buffer, slice)`
  = the `EmitCs(invalidateBuffer)` + `ThrottleDiscard` half, in ring order.

Draws recorded before the Map use the old slice and draws after it the new one, exactly as with DXVK on one thread:
the rename is ordered in the CS stream by the record's position. The app's writes into the slice happen before its
`Unmap` returns and before the draws that read them are recorded; those reach the GPU only through a later submission
(app writes → ring publish (release) → worker → CS queue → submit thread → `vkQueueSubmit`).

**No-overwrite (split).** Returns `D3D11Buffer::GetMapPtr()`, which the app thread itself last set. No record.

**Who else writes `m_mapPtr`.** DXVK sets it in `MapBuffer`, `UpdateMappedBuffer` (UpdateSubresource on a
host-visible buffer), `DiscardBuffer` (`Discard` of a host-visible buffer) and the initializer at creation. With the
frontend on, the immediate context's `Map` of a dynamic buffer only ever runs on the app thread, and `Discard` /
`UpdateSubresourceUP` whose destination is a dynamic buffer are routed synchronous, so `m_mapPtr` of a dynamic buffer
has a single writer at a time.

Slices handed out ahead of the worker are memory the ring does not bound: the app thread counts the bytes of
discards the worker has not passed to DXVK yet and drains above 64 MiB (DXVK's `ThrottleDiscard` bounds them from
there). A dynamic buffer without a CPU mapping (`D3D11_COMMON_BUFFER_MAP_MODE_NONE`) takes the synchronous map; a
lost device answers a null pointer and `D3DDDIERR_DEVICEREMOVED` (legal for `Map`).

Pre-allocated slices: after the worker replays the discard of a buffer of at most one allocator page, it allocates
that buffer's next slice from its own cache and parks it in the buffer (one reference, exchanged atomically). The
app thread's next discard takes it and does not allocate; without one it allocates from its own cache.

### 5.3 UpdateSubresourceUP, CopyResource, CopySubresourceRegion

- **Buffer destination** (`ResourceUpdateSubresourceUP`, `DefaultConstantBufferUpdateSubresourceUP`): the payload is
  copied into the record (box width, or the whole buffer without a box) when it is at most 64 KiB; larger payloads and
  dynamic-buffer destinations are synchronous. The app may reuse its memory as soon as the call returns, so the record
  must own the bytes; a reference with a fence is not possible without stalling the app later, and the copy is no
  more than DXVK makes into its own staging memory anyway.
- **Texture destination**: synchronous in phase 1 (sizing the payload needs the format/mip geometry); phase 2 copies
  it with the same 64 KiB bound.
- **Copies / resolve**: deferred; arguments are handles, subresources and an optional box (copied). The source and
  destination stay alive through the destruction rule (§5.5).

### 5.4 Queries, predicates, GetData

- `QueryBegin`, `SetPredication`: deferred.
- `QueryBegin`, `QueryEnd`: deferred; the wrapper stamps the record's end position into the query's own handle
  region (`QueryPrivate` gains `end_pos`), which only the immediate context reads. Stamping Begin too keeps a GetData
  from reaching DXVK between a queued Begin and its End (DXVK would answer `DXGI_ERROR_INVALID_CALL`, which the
  forward code reports as device removed).
- `QueryGetData` (split, app thread):
  - if the worker has not yet replayed the query's latest `Begin`/`End` (`done < end_pos`), answer "still drawing"
    (`DXGI_DDI_ERR_WASSTILLDRAWING`) without touching DXVK; S_FALSE is always a legal answer and the worker keeps
    making progress on its own;
  - otherwise call DXVK's `GetData` on the app thread **with `DONOTFLUSH`**: DXVK reads the query status lock-free
    from the query object, which its CS thread updates, so this is safe concurrently with the worker;
  - when the app did not pass `DONOTFLUSH` and the answer is pending, the flush DXVK would have done
    (`NotifyStall` + `ConsiderFlush`) is deferred instead: a "query flush" record replays
    `GetData(query, nullptr, 0, 0)` on the worker. `ConsiderFlush` may decline (few chunks, submissions in flight),
    and upstream DXVK recovers because every pending poll asks again, so every unflagged pending poll queues a new
    hint once the worker has run the previous one (the region remembers the last hint's position): at most one is
    outstanding per query, and a `while (GetData(q) == S_FALSE) {}` loop keeps asking exactly as it does with the front end off.
- A GetData `S_OK` for an event query still means "all work before End finished on the GPU": DXVK can only report it
  after replaying End, which the worker reaches after every record before it.

### 5.5 Object lifetime

Records reference objects by identity word, without references. Destruction must therefore wait until the worker is
past every record recorded before it.

- **Views, state objects, shaders, element layouts, queries, samplers, command lists** (no runtime callback in their
  destroy): the wrapper takes the identity word out of the region (the runtime frees it after we return), notes the
  current producer position `P`, and queues `(destroy fn, word, P)` on the frontend's destroy queue (mutex, rarely
  contended). The worker runs queued destroys once `done >= P`, with a pseudo-handle over the word. If the worker is
  already past `P` (idle), the destroy runs inline. The existing destroy bodies (cache eviction, `ia_gen` bump, binding
  shadow clears, COM release) are unchanged.
- **Resources**: `DestroyResource` calls `pfnDeallocateCb` (every resource has a WDDM allocation) and touches
  region-keyed registries, so it is synchronous: `wait_done(P)` then the unchanged destroy on the calling thread.
  Destroys come in bursts at level load, where the wait is cheap; a split variant (callbacks now, release on the
  worker) is phase 3 if counters show waits in play.
- **Free-threaded destroys**: under FREETHREADED caps the runtime calls `Destroy*` from any thread, concurrently with
  the producer. The runtime destroys an object only when its last reference (app and runtime bindings) is gone, so no
  thread is recording a use of it; every record that used it was published before the release that led to the
  destroy, and `P` (read with acquire) covers it.
- **Device destruction**: `DestroyDevice` drains, runs the remaining queued destroys, stops and joins the worker,
  then proceeds as with the front end off.

### 5.6 Views and state objects

Creation is device-level (inline on the calling thread); the new object's identity word is stored in its region
before `Create*` returns, and every later record snapshots it. Binding is deferred. Nothing else changes.

### 5.7 Flush

`pfnFlush` without cross-process shared resources is deferred, and the record calls only
`ID3D11DeviceContext::Flush` — never the forward `flush`, which re-reads the shared-resource list and could run the
flush gate (with `pfnRenderCb`) on the worker if another thread opened a shared resource in between. The
device-removed check and `scanout_acquire::retry_register` stay on the app thread. With shared resources
(`nvk_keyed_resources` non-empty) the flush gate's ordering is a synchronous contract (the runtime releases the keyed
mutex right after `pfnFlush` returns, and the gate may call `pfnRenderCb`), so it drains and runs inline.

### 5.8 Present, SetFullscreen, ResizeBuffers, the KMD present path

Every DXGI DDI that orders against rendering drains and runs inline. At `Present` the worker has replayed every record of the frame into DXVK, so the
present path sees exactly the state it sees with the front end off: `present_frame_gate` orders against DXVK's submission, the KMD
present (Blt / flip / `pfnPresentCb`), the scan-out snapshot, the Helios scan-out acquire (armed by DXVK at submit),
and the present-stream correlation are unchanged. `SetFullscreen` (`SetDisplayMode`), `ResizeBuffers` (destroys +
creates + `RotateResourceIdentities`) are DXGI calls or resource lifetime calls and synchronous.

The cost is that the app waits at `Present` for the worker's tail of the frame. Because the worker runs concurrently
with the app's recording, the tail is the worker's lag behind the app (the last records of the frame), not the frame's
front-end work. Deferring `Present` itself (letting the app start frame N+1 while the worker presents N) needs the
present callback on the worker (`pfnSetAsyncCallbacksCb`) and is out of scope.

### 5.9 Shared resources, KMT handles

Open/create of shared resources are device-level and unchanged. Their hand-off points (`Flush` with keyed resources,
`AcquireResource`/`ReleaseResource`, `ResolveSharedResource` / GetDC) are synchronous. Resource destruction is
synchronous, so region-keyed registries keep working.

### 5.10 Errors and device removal

`set_runtime_error` called on the worker (a deferred forward function failing) stores the HRESULT in the frontend
instead of calling `pfnSetErrorCb`, which is only valid inside a DDI call. Every wrapper checks the stash on entry
(one relaxed load). Most immediate-context DDIs may only report `D3DDDIERR_DEVICEREMOVED` (anything else is a
critical error to the runtime), so only that is reported, on the app thread through the device's `pfnSetErrorCb`;
other stashed errors are counted, logged and dropped. In `GetData` and `Map` a stashed removal wins over "still
drawing" / a mapping. `report_if_removed` in `Flush` and `Present` stays on the app thread, as does every DXGI-level
removed check.

### 5.11 Multithreaded apps

Several app threads may use the immediate context under the D3D11 device lock: each one in turn is the producer.
`D3D11_CREATE_DEVICE_SINGLETHREADED` apps promise one thread at a time. Threads making device-level calls, including
free-threaded destroys, never touch the ring's producer cursor; they use the destroy queue.

## 6. DXVK change (patch 0018)

`third_party/patches/dxvk/0018-helios-threaded-frontend-map.patch` adds to `D3D11ImmediateContext`:

- `m_heliosFrontendCache`: a second `DxvkLocalAllocationCache`, created like `m_allocationCache`;
- `HeliosFrontendDiscard(D3D11Buffer*, void** ppData) -> Rc<DxvkResourceAllocation>`: the app-thread half of
  `MapBuffer(WRITE_DISCARD)` (device-status check, `helios_feed::mapWriteDiscard`, `DiscardSlice` with the frontend
  cache);
- `HeliosFrontendInvalidate(D3D11Buffer*, Rc<DxvkResourceAllocation>&&)`: the worker half (`EmitCs(invalidateBuffer)`,
  `ThrottleDiscard` above a page).

The bridge exposes them as `fe_map_discard`, `fe_buffer_map_ptr`, `fe_emit_invalidate` and `fe_release_slice` on
`HeliosDxvkDevice`. Nothing else in DXVK changes; its immediate context is only ever entered by one thread at a time
(the worker, or the app thread while the worker is drained), except for the two lock-free paths above.

## 7. Knobs and counters

- `HKLM\SOFTWARE\Helios\UmdThreadedFrontend` (DWORD): 0 = off (default), 1 = on for every process except dwm.exe,
  2 = on including dwm.exe. Process override `HELIOS_D3D11_FRONTEND=0|1`.
- `UmdFrontendRingKiB` (DWORD, default 4096, clamped 256–65536).
- `UmdFrontendSync` (DWORD bitmask, default 0): call classes forced onto the synchronous path, for A/B runs that pin a
  rendering difference to one mechanism (`umd_common::cmd_record::FrontendSync`; the start-up log line names them):

  | bit | class | what changes |
  |---|---|---|
  | 1 | maps | dynamic-buffer DISCARD / NO_OVERWRITE maps drain and use DXVK's own `Map` on the app thread (no split) |
  | 2 | updates | buffer `UpdateSubresourceUP` drains and runs inline (no payload copy in the ring) |
  | 4 | destroys | view / state / shader / layout / query / command-list destroys drain and run inline (no retire queue) |
  | 8 | queries | `QueryGetData` drains, then the unchanged forward call (no early "still drawing", no hint record) |
  | 16 | no_park | the worker does not pre-allocate a buffer's next discard slice (patch 0018 `PreAllocate`) |
  | 32 | serial | every record is waited for before its DDI returns: same thread split, no overlap with the app |
  | 64 | flush | `Flush` drains and runs the forward flush inline |

  127 makes every class synchronous; with `serial` too, the only difference left from front end off is which thread
  enters DXVK.
- One summary line at `DestroyDevice` (and every 4096 drains with `UmdTrace`): records, bytes, sync calls and drains by
  reason, drain wait µs (sum, max), producer full-waits, worker parks/wakes, destroys queued/inline/waited, GetData
  early-pending, split maps (discard / no-overwrite), stashed errors.
- `UmdFrameTrace` (DWORD, default 0): non-zero writes one CSV line per Present of every front-end device to
  `C:\ProgramData\Helios\frametrace-<pid>.csv` (`umd_common::frame_trace`; a lock-free ring the presenting thread
  fills, drained by a `helios-frame-trace` thread every 250 ms). Per frame: `qpc_ms` (QPC ms at the Present DDI, the
  clock of PresentMon's `CPUStartQPCTime` with `--qpc_time_ms`), `interval_ms` (Present to Present on this thread),
  `present_drain_ms`, `ddi_calls`, and the presenting thread's blocked time by wait point: `sync` (drains of the other
  synchronous calls, destroys, discard back-pressure), `map` (whole synchronous Map/Unmap, drain and DXVK map
  included), `getdata`, `flush`, `full` (pushes into a full ring), with counts; `worker_busy_ms` (time the worker ran
  records) and `blocked_ms` (their sum with the Present drain). `tools/pm-sample-split.py PM.csv --trace FILE`
  compares them between slow and fast frames.

## 8. Phases

1. **Core + hot path** (implemented): ring + worker + destroy queue + error stash in `umd_common` (host unit tests);
   deferred draw / bind / state / constant-buffer / clear / copy / query / flush paths; split buffer map; split
   GetData; synchronous everything else with the `d3d11_context` safety net; DXVK patch 0018 (with the worker's
   pre-allocated discard slices).
2. Texture `UpdateSubresourceUP` payloads; `CommandListExecute` if CS2 shows it (until then synchronous).
3. Split resource destruction; tune spin/park; consider deferring `Present`.

## 9. Measuring

- `d3d11_drawbench` (`guest/windows/tools`): submit µs per frame and µs per draw with the frontend off/on
  (`HELIOS_D3D11_FRONTEND=0/1` in the same package, alternating runs), default shape `10 2000 2 8 256`, plus a
  4000-draw run.
- Host: `cargo test --release --lib -- --ignored --nocapture producer_cost` in `umd_common` measures the ring's
  producer cost (about 15 ns per 64-byte record on the build host, the worker consuming concurrently).
- CS2 (by the owner): fps and the render-thread profile (`thread-sample.ps1`), worker thread share, `dxvk-cs` share.

### Results

`d3d11_drawbench` `present` mode (flip swap chain, Present every frame), win11 on 2 MiB pages, alternating runs in the
same package:

| mode | off: µs/draw, frame sd | on: µs/draw, frame sd |
|---|---|---|
| real, 2000 draws | 1.08 / 1.00, sd 18% / 12% | 0.95 / 0.90, sd 18% / 12% |
| hot, 2000 draws | 0.29 / 0.30, sd 14% / 27% | 0.46 / 0.43, sd 11% / 11% |

The game-like load (`real`: thousands of distinct objects, varied binding order) is 10–14% faster with the front end
and paces the same; the cache-hot loop is slower (record and replay cost more than the inline path when everything
stays in cache). CS2's front-end cost is cold-miss bound like `real`.

CS2 demo playback (1600x900, alternating runs): 268–272 fps average with the front end, 261–268 fps without; the
slowest second 142–193 fps with it, 84–99 fps without. The front end evens out the frames more than it raises the
average.

At Present the render thread spins (up to 4 ms) instead of sleeping while it waits for the frame's tail, and the
worker spins 100 µs before parking, so a frame boundary is not a sleep/wake handshake.

## 10. Invariants

The design above depends on these; each is handled where the section says:

- a deferred `Flush` record calls only `ID3D11DeviceContext::Flush`, never the forward `flush`, which could run the
  shared-resource gate (`pfnRenderCb`, the `Cell` command windows) on the worker (§5.7);
- every pending, unflagged GetData poll re-hints a flush, one outstanding per query, so a spinning app cannot hang
  when `ConsiderFlush` declines (§5.4);
- the safety net is a backstop, not the guarantee: bridge entry points use the context directly, so every DXGI and
  shared-resource path drains explicitly (§4);
- `QueryBegin` stamps its position too, so a GetData between a queued Begin and its End is never reported as device
  removed (§5.4);
- only device removal is reported from a stashed error; other stashed errors are counted and dropped (§5.10);
- discarded slices handed out ahead of the worker are bounded at 64 MiB; the map mode is checked; a lost device
  answers a null mapping (§5.2);
- process exit, `DISABLE_EXTRA_THREAD_CREATION` and `UmdFreeThreaded=0` never get or never wait on a worker (§3);
- the recycle DDIs are free-threaded and stay device-level; `Discard` of a host-visible buffer writes `m_mapPtr` and is
  synchronous (§4, §5.1).

## 11. Risks

- A forward function that keeps hidden per-thread state or calls a runtime callback from a deferred path. Audited:
  the only `thread_local!` is the dcomp vehicle slot (present path, synchronous); callbacks are listed in §4.
- The safety net drains when an app-thread path reaches `d3d11_context` unexpectedly: correct but slow. The summary
  line counts drains by reason so such a path shows up.
- Worker and `dxvk-cs` both near a core each: the frame is then bound by the slower of app / worker / CS; with 16
  vCPUs there is room, but the guest's 4 KiB host pages (§1h) make every thread latency-bound.
