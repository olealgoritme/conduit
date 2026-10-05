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

Base: `winboat-org/mesa-helios` @ `89bd0676a4e69740d9900fb13561b14acc4997d6`.
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
ever sees an unflushed entry. The queue is flushed by that, before every other
escape, at the entry cap, and by a flusher thread a fixed time after the queue
became non-empty.

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
