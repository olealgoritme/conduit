# icd/patches — changes to the vendored Mesa venus ICD

`third_party/mesa` is `winboat-org/mesa-helios`, a separate repository. Changes
that belong in it are kept here as patches against a named upstream commit, so
they can be reviewed and A/B-tested without touching that repository.

Apply in the Mesa tree the ICD is built from (the VM's `Z:\icd\mesa`):

```
git apply guest/windows/icd/patches/0001-helios-defer-signal-batches.patch
```

then rebuild `vulkan_virtio.dll` and install it as usual
(`tools/install-helios-icd.ps1`).

## 0001 — defer small signal batches into one batch escape

Base: `winboat-org/mesa-helios` @ `89bd0676a4e69740d9900fb13561b14acc4997d6`
(`vn_renderer_helios.c` and `vn_renderer_helios_producer.h`).
Needs the KMD verb `HELIOS_ESCAPE_SUBMIT_VENUS_BATCH` (driver ≥ 22.22.300.0);
against an older KMD the probe fails and the ICD behaves exactly as before.

**Why.** In steady state every renderer submit the ICD makes is a small
`vkWaitRingSeqnoMESA` waiter that signals a semaphore
(`vn_signal_win32_external_semaphore`, `vn_create_sync_file`); command streams
themselves travel through the shared ring and cost no escape. Each of those
waiters was one `D3DKMTEscape` (a WoW64 thunk plus dxgkrnl on a 32-bit caller),
about 2000 a second under Heaven.

**What.** A submit with `cs_size <= 256`, at least one sync (at most 4) and no
present tag is queued instead of sent. The queue goes out as ONE batch escape,
which the KMD fans out as N ordinary fenced SUBMIT_3Ds in order — one wire fence
per entry, so ordering and fence semantics are those of N single escapes.
Anything else (a ring notify, a tagged present, a larger stream) is sent
directly, after the queue. The queued entries' syncs are appended to at once with
a placeholder fence id that the flush patches to the real wire id in place, and
shared (WDDM) syncs reach the retire thread then.

**Why it is safe.** What is deferred carries no GPU work, so deferral cannot
reorder execution — it only delays *when a semaphore signals*, by at most the
flush bound. The invariant that keeps the rest simple: every `dev_mutex`
acquisition except `ops.submit`'s own flushes the queue first, so no other code
ever sees an unflushed entry (and an escape issued by a lock holder is ordered
after it). `vn_renderer_helios_producer.h` takes that mutex directly, so it is
patched to use the same lock. The escapes that deliberately take no lock — fence
waits and events, the GPU-fence barrier — need no ordering against queued
waiters and are left alone, so they never wait on the mutex. The queue is
flushed on every lock acquire, before a direct submit, at the entry cap, and by
a flusher thread a fixed time after the queue became non-empty.

A flush that fails (the escape, an entry, or the retire hand-off) latches the
renderer lost: later submits and waits fail with `VK_ERROR_DEVICE_LOST`, and the
affected syncs are left incomplete rather than signalled for work that never
ran.

**Default OFF.** DWM loads this same ICD, so nothing changes unless a process
opts in. Per process:

| variable | meaning | default |
|---|---|---|
| `HELIOS_SUBMIT_BATCH=1` | enable (needs the new KMD) | off |
| `HELIOS_SUBMIT_BATCH_MAX` | flush at this many entries (2–64) | 16 |
| `HELIOS_SUBMIT_BATCH_US` | flush this long after the queue became non-empty (50–5000 µs) | 300 |

**Measuring.** KMD counters (registry, under the service key): `EscCalls` =
submission escapes received, `EscSub` = submits accepted, so `ΔEscSub/ΔEscCalls`
is submits per escape and `ΔEscCalls/Δt` is escapes per second; `EscBat`,
`EscBatEnt`, `EscBatMax` describe the batch verb. ICD side: diag lines
`submit-batch: ...` (every 8192 flushes and at exit) and, with `HELIOS_PERF=1`,
a `submit_batch` line in the perf summary (`flushes`, `entries`, `direct` =
submits sent directly while batching is on, `max`).

Compare Heaven with and without `HELIOS_SUBMIT_BATCH=1` in the Heaven process's
environment only.

## 0002 — survive the KMD stopping under a live process

Base: `winboat-org/mesa-helios` @ `89bd0676a4e69740d9900fb13561b14acc4997d6`;
also applies on top of 0001. Listed in `series`, so `ci/windows/build-mesa.sh`
applies it to the shipped ICD (0001 stays manual).

**The crash.** At every live driver update (pnputil) dwm.exe, explorer.exe and
ApplicationFrameHost.exe died with 0xC0000005 at `vulkan_virtio.dll+0x3858c4`.
That is `vn_ring_submit_locked` (`vn_ring_write_buffer`/`vn_ring_has_space`
inlined), the load `mov (%rax),%r14d` of `*ring->shared.head`. The ring shmem
is a MAP_BLOB view the KMD maps into the process itself
(`MmMapLockedPagesSpecifyCache(UserMode)`); when the KMD stops, dxgkrnl
destroys every device and `DxgkDdiDestroyDevice` unmaps those views. The ring
writer makes no escape, so the first thing that notices is the next ring
access, on a free VA.

**What.**
- `vn_renderer_helios_lost.h`: every KMD-made mapping (ring and cs/reply
  shmems, mapped bos, the producer status page) is registered with its owning
  renderer. A vectored exception handler catches an access violation inside a
  registered range whose VA is now free, commits zeroed memory at exactly that
  range, latches the renderer lost (`vn_renderer::lost`), reserves that
  renderer's other freed ranges too, and resumes the faulting instruction. A
  range that is still mapped, or that something else took, is left alone and
  the fault goes to the next handler as before.
- A D3DKMTEscape failure with a device-gone status (`STATUS_DEVICE_REMOVED`
  and friends) latches the same way, before the next touch. Once lost, escapes
  are not issued any more.
- Once lost: the ring reports `FATAL` without reading shared memory, ring
  submits and space waits fail instead of spinning into `vn_relax()`'s abort,
  `vkQueueSubmit*`, `vkGetFenceStatus`, fence/semaphore waits and the Helios
  submit/wait ops return `VK_ERROR_DEVICE_LOST`, the retire thread stops
  slicing. The client (the UMD under DWM) takes its device-removed path.

**Test.** `src/virtio/vulkan/test_helios_lost.c` (not built by meson):
pagefile-section views stand in for KMD views and are unmapped under four
threads that keep writing to them like the ring writer.

```
x86_64-w64-mingw32-gcc -O2 -static -o test_helios_lost64.exe test_helios_lost.c
i686-w64-mingw32-gcc   -O2 -static -o test_helios_lost32.exe test_helios_lost.c
```

Both print `PASSED` in the win11 guest. The real check is a live driver swap
with the patched ICD installed: dwm/explorer must not fault in
`vulkan_virtio.dll`, and `C:\ProgramData\Helios\helios_icd_diag.log` should show
`device-lost: access violation at ... backed with zero pages, renderer lost`
or `device-lost: D3DKMTEscape status=0xc00002b6 ...` for each process that had
the ICD loaded.

## 0003 — one KMD-view loss table for the ICD and the UMD

On top of 0002 (in `series`). Replaces 0002's per-module
`vn_renderer_helios_lost.h` with `helios_kmdmap.h`, a byte-identical copy of
`guest/windows/umd_common/bridge/helios_kmdmap.h`, which `helios_umd.dll`
compiles too (`umd_common/bridge/bridge_kmdmap.cpp`).

**Why.** At the 319.1 -> 319.2 live swap with 0002 installed, Explorer,
ApplicationFrameHost and StartMenu survived, but dwm.exe died in
`helios_umd!helios_scanout_ledger_snapshot_v2+0xca` (`mov eax,[r8+8]`, the
ledger's `slot_count`): the scanout read ledger is a KMD view of the UMD's own
that the KMD unmapped at DestroyDevice. The ICD log also showed
`cannot back freed range ... (err=487)`. The dump explains it: a 4 KiB
allocation had already landed at the second 64 KiB granule of a freed 132 KiB
ring view of ANOTHER renderer, which 0002 neither latched nor swept (a loss
only latched the renderer whose range faulted).

**What.**
- One process-wide table (named section `Local\HeliosKmdMap-<pid>`, created
  atomically by whichever module comes first) holds every KMD view of every
  module. Each module installs its own vectored handler, and every handler runs
  the same code on that table under the table's lock, so it does not matter
  which one runs first and they cannot fight.
- Loss is a process-wide **epoch**: every user (ICD renderer, UMD device)
  records it at creation; one loss loses every user alive then (one KMD serves
  the process), and users created after the KMD restarts start clean. A
  straggler fault in an old generation's range does not move it again.
- The first fault, or a device-gone escape status, backs every registered range
  that is already free, in all modules, before anything else can take it.
- When part of a freed range was taken anyway, backing goes granule by granule
  (the err=487 case) and the taken granule is left alone.
- UMD: the ledger view is registered at map, unregistered before UNMAP, and the
  readers (`helios_scanout_ledger_lookup_v2` / `_snapshot_v2`) skip a view
  whose device is lost (they report "no ledger", which callers already handle).

**Test.** `guest/windows/icd/win-build/helios_kmdmap_test.c`, with
`helios_kmdmap_mod.c` as a second module (DLL) in the same process: ring views
unmapped under four writer threads, a ledger page owned by the other module, a
freed ring with a squatter in its middle granule, a still-mapped view, a range
wholly taken, the escape-status path, and a user attached after the loss. Build
commands are in the file; x64 and x86 both PASS in win11. The Mesa copy must
stay identical: `cmp guest/windows/umd_common/bridge/helios_kmdmap.h
<mesa>/src/virtio/vulkan/helios_kmdmap.h`.

## 0004 — a lost device touches none of its mappings

On top of 0003 (in `series`).

**Why.** At the 319.4 -> 320.1 live install dwm.exe died at
`vulkan_virtio.dll+0x2ac650`: `vn_CreateFence` writing the initial status into
a fresh fence's feedback slot, called from `vn_QueueWaitIdle` (it lazily
creates its wait fence) under `vk_common_DeviceWaitIdle`, from the UMD
destroying a D2D device after the loss. The loss had been caught by the escape
path (`STATUS_DEVICE_REMOVED`) before the KMD unmapped the views, so the sweep
at that moment found the feedback buffer still mapped. By the time of the write,
4 s later, its VA held a read-only 12 KiB `MEM_MAPPED` view (the restarted D3D
device), and both modules' handlers correctly declined it (`declined=2` in
the shared table in the dump).

**What.**
- Venus does not touch feedback slots or mapped memory once the device is
  lost:
  - no new feedback slots are created (fence, semaphore and event creation
    fall back to the non-feedback path);
  - `vkQueueWaitIdle` (and through it `vkDeviceWaitIdle`), fence status,
    `vkGetEventStatus`, `vkSignalSemaphore` and `vkGetQueryPoolResults` return
    `VK_ERROR_DEVICE_LOST`;
  - `vkResetFences`, `vkSetEvent`/`vkResetEvent` and `vkResetQueryPool` skip
    their slot writes;
  - coherent-cached flush/invalidate are skipped.
- `helios_kmdmap.h`: every `helios_kmdmap_lost()` that answers "lost" re-sweeps
  the old generation's unbacked ranges (once per tick, never waiting for the
  lock), and every escape attempted on a lost renderer calls it. A view the KMD
  unmaps after the loss was noticed is then claimed within one tick of the next
  touch of the lost renderer, instead of being left free for anything else.
  The UMD's ledger readers call it too.

**Test.** `helios_kmdmap_test.c` case 5: the loss is marked while the view is
still mapped, the view is then unmapped, and the next `lost()` check backs it.
x64 and x86 PASS in win11.
