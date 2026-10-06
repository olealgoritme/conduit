# Foreign scanout source (KMD-driven zero-copy flips)

Status: implemented on `kmd/foreign-scanout`, never built or run (the KMD cannot be
compiled where it was written). The pure logic is host-tested; everything else was
reviewed by hand. Contract: `protocol/src/nvrm_scanout.rs`, mirrored in
`guest/rmclient/src/helios_nvrm_escape.h`.

## Problem

A process that forwards host `ScanoutFlip` (msg 20, see `docs/SCANOUT.md`) through
`HELIOS_NVRM_OP_FORWARD` shows its image, but the desktop keeps flipping scanout 0
through Venus, and the host viewer shows whichever flip came last. WDDM's own flip
bookkeeping knows nothing of the app's frames.

## Design

Three more `HELIOS_ESCAPE_NVRM` ops (not a new verb: the owner proof is the same
DRM-node handle table, and librmclient already includes the header):

| op | |
|---|---|
| `SCANOUT_SET` (9) | `{handle, width, height, stride, offset, fourcc, modifier, lapse_ms}`. `handle` must be the caller's own DRM-node handle (`device_type >= 512`, as the FORWARD `ScanoutFlip` arm requires). Layout validated once. Out: `out_generation`, effective `lapse_ms`. |
| `SCANOUT_PRESENT` (10) | `{handle, gem}` per frame. The KMD builds the 64-byte `ScanoutFlip` from the stored layout, mints `seq`, sends it. Out: `out_seq`. |
| `SCANOUT_RELEASE` (11) | `{handle or 0}`. Idempotent. |

State machine (`kmd_logic::foreign_scanout`): `Inactive`, `Active{owner, handle,
epoch, generation, layout, lapse}`, `ReleasePending`. One source at a time (scanout 0).
`SET` by another device while the holder presented within its lapse is
`SCANOUT_BUSY`; `SET` by the same device updates in place. `seq` is strictly
increasing across all sources of the boot.

While `Active` and not lapsed, the desktop's host flush is withheld: in
`queue_active_scanout_refresh_locked`, after every "is anything bound / busy" arm
and before the ownership gates, `foreign_scanout_suppresses()` is asked. If it says
yes the refresh is dropped like an ownership-gate drop (armed id cleared,
publication transaction cancelled, leases ended) and `Dropped` is returned.
Only `RESOURCE_FLUSH` is withheld (on the host only a flush of the scanout resource
makes the viewer show it; `SET_SCANOUT_BLOB` shows nothing by itself). Binds, the
present path, WDDM DMA fences, vsync, windowed-blt completion and the read ledger
(issued per flush token, so nothing is issued) are untouched: a suppressed desktop
present completes exactly as before, there is simply no host read to wait for.

The source ends, and the desktop owes one fresh flush
(`ReleasePending` -> `request_scanout_refresh()`, retired when the worker queues a
flush or finds nothing bound), on:

* `RELEASE`;
* a successful forwarded `Close` of the source's DRM file (`nvrm::forward`);
* device teardown: the entry of DestroyDevice (before its sweeps) and `close_all_for_owner` (see
  "Owner death and killed processes");
* the suppression gate finding the handle no longer the owner's, or the NVRM epoch
  changed (re-checked on every suppressed refresh, so a missed hook cannot wedge it);
* the lapse: no `PRESENT` for `lapse_ms` (default 2 s, 100 ms..30 s). The HPD worker
  waits with a timeout equal to the remaining lapse while a source is live, so a
  silent owner gives the desktop back with no other edge. A DISPATCH-level watchdog on the vsync tick ends a source the worker has not
  polled 250 ms after its deadline.
* transport reset / StopDevice (`reset_display_publication_state`): `Inactive`, no
  restore (the display state is rebuilt).

A flip that is in flight when the source ends can land after the restore request;
`PRESENT` notices (`foreign_scanout_flip_done`) and requests one more desktop flush,
which is ordered after it.

**The KMD's own resident source** (`KmdRmClient` = 3, `kmd-rm-client.md` section 13) is a second
kind of source with the lower priority of the two: it has no lapse, a user `SCANOUT_SET`
preempts it at once (the user source becomes the `Active` one; the resident registration is kept),
and when the user source ends by any of the paths above the resident source takes scanout 0 back and
the restore is a re-flip of its surface instead of a desktop flush. It is counted apart (`Rm*`), so
the live-source arithmetic of the counters below stays a count of user sources. How it composes
with fenced presents (`rm-fence-marker.md`: a user source's queued flips, preemption, resume, every end
path) is the state machine of `kmd-rm-client.md` section 13.12.

A forwarded `ScanoutFlip` (FORWARD) from a device that does not hold a live source is
refused `FORBIDDEN`; with no source live FORWARD flips behave as before (and race the
desktop, as before).

## Fenced presents

`PRESENT` can also queue the flip behind an RM fence and return at once; the flip is sent
by the HPD worker when the fence fires (flags bit 0, `rm_fence_handle` at offset 52,
capability bit `HELIOS_NVRM_CAP_SCANOUT_FENCE`). See `rm-fence-marker.md` (ordering, the
image reuse rule, ownership of the fence handle, counters).

## Buffer release (`NVGPU_F_SCANOUT_RELEASE`, `SCANOUT_STATUS`, `SCANOUT_RELEASED`)

Status: implemented on `kmd/scanout-release`, never built or run (the KMD cannot be compiled
where it was written); the pure rules are host-tested (`kmd_logic::scanout_release`, the
presenter in `kmd_logic::rm_present`). Host contract: the host's `docs/SCANOUT.md` ("Buffer
release", branch `feat/host-s6-flip-release`).

**Why.** A host flip has no completion: `PRESENT` returning, or even the host's reply, says
nothing about when the viewer (or the encoder) stops reading the previous image. Until now the
only rules were "write another image than the last one flipped" (the RM ring presenter) and
"not before the next frame's fence fired and its call returned" (`rm-fence-marker.md`): both
guesses, both able to tear. The host now says when it is done.

### Negotiation

The device offers the virtio feature `NVGPU_F_SCANOUT_RELEASE = 1 << 15` (config `features`
bit 15 is unused) whenever it has a display. `VirtioGpu::init` acks it iff the device offers it
AND the display half is on (`DisplayHalf`, the knob `StartDevice` passes in): every consumer
lives in the display half, and a render-only start acks nothing new, so the host keeps no
bookkeeping for it. The ack is part of the one `FEATURES_OK` write; a device that does not leave
`FEATURES_OK` set for the larger set is reset and offered the required set alone
(`negotiate_features`, counter `RelNeg`), so the optional bit never costs the transport.
`VIRTIO_F_VERSION_1` stays the only required bit. Bit 12 (`TAKES_INPUT`) is still NEVER acked
(`helios_protocol::NVGPU_F_TAKES_INPUT`; a const assertion in `gpu/nvrm_events.rs` keeps it out of
both feature sets). Counters: `RelAck` (1 = acked), `RelNoQ` (acked but the event queue is not up:
nothing can be delivered, the consumers stay off).

A host that does not offer the feature (conduit-vmm offers only `VERSION_1`, an older backend, a
backend with no display) leaves every behaviour as it was: no event arrives, `QUERY_CAPS` has no
release bit, `SCANOUT_STATUS` answers `UNSUPPORTED`, and the ring presenter alternates its two
surfaces by the old rule. A transport reset clears everything below (`foreign_scanout_reset` ->
`scanout_release::reset`); the next `StartDevice` turns tracking on again if the new transport
acked it.

### The event

Event queue (virtqueue 1) message `ScanoutReleased` = 28: the 16-byte `MsgHeader` (handle 0,
status 0) and 32 bytes `{u32 scanout (0), u32 flags, u32 owner_handle, u32 host_handle, u64 seq,
u64 reserved}`; flags `RESOURCE` 1, `NOT_SHOWN` 2, `FORCED` 4. Meaning: the buffer's latest flip
(`seq`) was replaced (or the scanout disabled) and every display client it was sent to is done
with it; the buffer on the scanout is never released, a re-flipped buffer is released again later,
a buffer whose GEM handle or file the guest closes is forgotten with no event. The 256-byte posted
buffers hold it whole. `drain_nvrm_events` (the DPC, under the virtio lock; no allocation) parses
it (`scanout_release::parse`) and gives it to the flip book; a reposted buffer is NOT kicked for a
release (a doorbell per frame is a VM exit; the host looks for a posted buffer every 2 ms).
`RESOURCE` (a Venus `SET_SCANOUT_BLOB` resource) is counted only (`RelRes`); see the read ledger
below. Counters: `RelRecv RelMatch RelDrop RelRes RelNotShown RelForced RelBad RelUnasked`
(`RelDrop`: named no flip the KMD minted, e.g. a forwarded `ScanoutFlip` or a buffer already
forgotten).

### The flip book (`kmd_logic::scanout_release::ReleaseBook`)

A fixed table (32 entries, leaf spinlock `BOOK`, DPC-safe) of the flips the KMD minted and sent:
`(seq, owner, drm handle, gem)`, each `Queued` (minted; in the fenced queue or in flight) ->
`OnHost` (the host took it) -> `Done`. A flip is `Done` when the host released its buffer; when
the SAME buffer was flipped again (the later seq carries the buffer; the host's event names the
latest); when it never reached the host (skipped for a newer ready frame, dropped with its source,
refused: `Drain::gone_seqs`); or when it has been `OnHost` for 2 s after it was replaced with no
event (a closed GEM gets none; a stuck entry must not hold the numbers back for ever). The current
buffer never ages. A full table overwrites its oldest done entry, else its oldest (`RelEvict`).
Entries are forgotten when their DRM file is closed, their device destroyed or the transport
reset. `floor(handle)` is the highest `S` such that every flip of that handle with `seq <= S` is
done: contiguous, hence conservative (a slow buffer delays the images flipped after it, never the
other way).

Ordering the book is robust against: the host sends a buffer's release BEFORE the reply of the
flip that replaced it, so the event can reach the DPC while the flip's sender is still waiting for
its reply. A release only needs the entry to exist (it was minted before the next flip), and the
`sent` that follows reports whether it finished older flips of the same buffer, in which case the
owner is woken again (otherwise a waiter woken by the event would read a floor the pending `sent`
was about to move, and sleep through it). A send that times out is assumed taken (the image stays
reserved until the host says otherwise or it ages out).

### The RM ring presenter

`kmd_logic::rm_present::Presenter` replaces "never write the surface flipped last" with: write
the surface that was NOT flipped last only after the host released its last flip. It remembers
each surface's latest flip seq (`note_seq`, from `present_within`'s return), and when a frame is
due and `Inputs.release_tracked` it asks (`back_wait_seq`, answered from the book by
`virtio/rm_present.rs`) whether that flip is done:

* done: copy and flip as before;
* not done: hold the frame (still owed, `Act::WaitUntil`) until the release arrives (the DPC wakes
  the HPD worker when the event retires a flip of the KMD's owner) or until 500 ms after the
  surface was replaced (the host's own `FORCED` limit, `RING_WAIT_100NS`): then write anyway and
  count it. Counters `RelRWaits` (a wait began, once per episode) and `RelRTimeouts`;
* a surface never flipped, and a ring of one surface (whose back surface is the one on screen and
  can never be released), do not wait; a resume re-flip writes nothing and never waits;
* a host without the feature (`release_tracked` false) is the old rotation, unchanged.

With two surfaces the release of the back surface normally arrives with the previous flip (no
client to wait for: before that flip's reply), so the wait costs nothing in steady state; it bites
exactly when a viewer or an encoder holds a buffer.

### What user mode gets

Capability: `QUERY_CAPS.supported_ops` bit 35 `HELIOS_NVRM_CAP_SCANOUT_RELEASE`, set iff the
transport acked the feature (with it: op bit 12 and `supported_event_kinds` bit 3). Probe the
capability, never the op bit alone. Without it keep the old rule.

**`SCANOUT_STATUS` (op 12)**, 64 bytes, `HeliosNvrmScanoutStatus` (C: `helios_nvrm_escape.h`):

| offset | field | |
|---|---|---|
| 0..40 | `HeliosNvrmHeader head` | `epoch` returned as for every op |
| 40 | `u32 handle` | in: the DRM-node handle given to `SCANOUT_SET` (a handle of the caller; `NOT_OWNED` / `FORBIDDEN` as for `SET`) |
| 44 | `u32 flags` | in: zero (`BAD_RANGE` otherwise) |
| 48 | `u64 out_released_seq` | out: every flip of `handle` with `seq <=` this is done; 0 = none |
| 56 | `u64 out_last_seq` | out: the newest `seq` the KMD remembers for `handle` |

Statuses: `OK`; `UNSUPPORTED` (no release feature); `NOT_OWNED` / `FORBIDDEN` (handle); `BAD_RANGE`
(flags); NTSTATUS `STATUS_DEVICE_NOT_READY` with no transport. No live source is needed (a lapsed
source can still be drained). A handle the KMD has no flips of answers the highest seq ever minted
in both fields.

**Client rule:** an image whose latest `PRESENT` returned `out_seq == P` may be written again once
`out_released_seq >= P`. Never true for the image on screen. This replaces, where the capability
exists, the heuristic of `rm-fence-marker.md` ("do not write the image of P before P+1's fence fired
and P+1's call returned"), and it also covers a fenced present that was skipped or dropped (it is
done at once). One image per flip suffices: with N >= 2 images a client waits only when the host
or a client still reads the image it wants.

**`SCANOUT_RELEASED` event (kind 3)**, `EVENT_REGISTER` with `handle = 0` (like `TRANSPORT_LOST`),
once per process; replaces by `(owner, kind)`; per-process bound is the handle bound + 2 (`rm_limits::EVENTS`,
`nvrm-escape.md` section 13.8). Signalled when `out_released_seq` may have advanced for that
process's flips (a release matched one of its flips; a queued flip was skipped, dropped or refused;
an older flip of a re-flipped buffer was finished); also by `TRANSPORT_LOST`'s wake-all. It is a
doorbell, not a count: a spurious wake costs one `STATUS`. Without the feature `REGISTER` of kind
3 answers `UNSUPPORTED`. The wait protocol that loses no wakeup (the event is not latched):

```
register kind 3 (handle 0) once
for each image to reuse (latest present seq P):
    reset the event (or use an auto-reset event)
    SCANOUT_STATUS  -> if out_released_seq >= P: go
    wait on the event, with a timeout (>= 600 ms is past the host's 500 ms forced release)
    SCANOUT_STATUS again; a timeout with the transport up (same `epoch`) means: proceed (tearing is
    the price of a host that stopped answering) and count it
```

Counters: `RelSig` events signalled, `RelTrack` flips entered, `RelGone` retired without an event,
`RelEvict`.

### The read ledger: assessed, not wired

A Venus `RESOURCE` release could in principle retire the desktop's scanout read in
`adapter/read_ledger.rs`. It is not a clean fit and is not done: the ledger retires a read at the
host's FLUSH reply per flush token, and a desktop that flushes ONE resource over and over (GDI, a
single-buffered primary) is never replaced, so it is never released; making the ledger wait for
the release would pin the front buffer's entry for ever and stall every UMD acquire that waits on
it. A flip-model swap chain (two or more resources) is where a release is exact; wiring it needs a
per-resource "latest flush token" next to the release (resource id -> token), the retire moved to
`max(flush reply, release)` only for resources that have a successor, and a timeout fallback like
the ring's. Left for the slice that removes the Venus flush (`kmd-rm-client.md` slice 3), where the
RM ring replaces this path anyway. Venus resource releases are counted (`RelRes`) so the traffic
is visible.

### Failure modes

* A release lost or never sent (a closed GEM, a host bug): the book ages the entry out 2 s after
  its replacement; the ring waits at most 500 ms per surface and counts it (`RelRTimeouts`, should
  read 0 on a healthy session; climbing means the viewer or encoder is slow or the host does not
  send them).
* A host that sends `ScanoutReleased` without the ack: counted (`RelUnasked`, `NvEvOther`), dropped.
* The event queue is full of releases nobody can read (DPC starved): the host retries every 2 ms; the
  DPC drains at most a ring's worth (16) per interrupt.

## Counters (`publish_nvrm_counters`, also on SET/RELEASE/lapse)

`FsSet FsPres FsRel FsLapse FsEnd FsTake FsSupp FsRest FsRef FsErr`. Live sources =
`FsSet - FsRel - FsLapse - FsEnd - FsTake` (0 or 1). `FsSupp` rising with no source
live is a bug. `FsRest` should track `FsRel + FsLapse + FsEnd`.

These are registry MIRRORS, written only at edges (SET, RELEASE, a worker lapse, an owner
exit, the `Nv*` mirror the worker runs after a session-shaping NVRM call, StopDevice). A value
that "did not change over 10 s" says only that no such edge happened, not that the live
counter stood still: read `FsPubT` (below) against the uptime before drawing conclusions. The
owner-death counters and breadcrumbs are in "Owner death and killed processes".

### Reading vsync rates

The heartbeat has exactly one source: `service_vsync_tick` (`adapter/kobj.rs`), run by a
one-shot fixed-phase timer at the display rate (41667 units of 100 ns at 240 Hz, 166667 at
60 Hz). It never catches up, so it does not burst. Its tick count is mirrored into the
service key from three places, at different times, which is why a count read from one
mirror and divided by wall-clock time (or by another mirror's time) is not a rate:

| count | its time | written by | when |
|---|---|---|---|
| `ScVs` | `ScVsT` | `enum_cofunc_modality` (`ddi/vidpn.rs`) | each call, dozens during a mode set, then rarely |
| `VpVsN` | `VpVsT` | `scanout_trace::dump` | first HPD worker wake, then every 128th |
| `VsCnt` | `VsCntT` | `pacing_snapshot` (`adapter/scanout.rs`) | about every 600 refreshes |

Every `*T` value is the interrupt time, in milliseconds (wraps at 2^32 ms, 49.7 days), of
the tick that last advanced the count beside it. `VpDmpT` is the interrupt time of the
`VpVsN` dump itself, on the same clock. The tick-gap statistics are also written by the dump:

* `VsMinGap`: the smallest gap between two consecutive ticks since boot, in 100 ns units
  (`0xFFFFFFFF` = none measured yet). The first tick after an arm is ignored (arm and
  disarm forget the previous tick), so a D3 round trip does not show up as a gap.
* `VsFast`: ticks that came closer than half a period to the previous one. A late tick is
  followed by one on the original phase, so a few are normal after a DPC latency spike; a
  burst would show `VsFast` close to the tick count. Ticks while `ControlInterrupt` has the
  delivery gate closed (`VpVsEn` 0) count here but do not advance the count or its time.

Recipe, using only values written by one dump (or one `enum_cofunc_modality` call):

1. Read the pair `(count, time)` twice, far enough apart that a dump happened in between:
   `VpDmpT` must have changed. Two reads inside one dump interval return the same values
   and a rate from them is undefined (zero elapsed time), not 0.
2. Rate = `(N2 - N1) / ((T2 - T1) / 1000)` ticks per second, with `N`/`T` from the same
   mirror (`VpVsN`/`VpVsT`) in both reads, or across mirrors with each count paired to its
   own time (`ScVs`/`ScVsT` against `VpVsN`/`VpVsT`). Differences are wrapping `u32`.
   `kmd_logic::vsync_rate::rate_mhz` is this arithmetic (millihertz: 240000 = 240 Hz).
3. Sanity: `VpDmpT - VpVsT` (`vsync_rate::age_ms`) is under one period on a running
   heartbeat (about 4 ms at 240 Hz); seconds mean it stalled or the gate is off. The rate
   should match the mode's refresh (`VpRfr`, millihertz) within a fraction of a percent;
   a result thousands of times off (for example 4000/s against a 240 Hz mode) means the
   count and time did not come from the same pair, or the machine rebooted between reads
   (the registry keeps the previous boot's values until the first dump).
4. `VsMinGap` close to the period and `VsFast` near 0 (against `VpVsN`) confirm no burst.

`VpVsN` counts only the ticks DELIVERED to dxgkrnl (the `ControlInterrupt` gate open), so a rate far
below the mode's refresh is either an idle dxgkrnl that keeps the gate closed or a slow timer.
`VsTickN` (every tick, whatever the gate) and `VsOffN` (ticks with the gate closed), written by
the same dump, tell which: `VsTickN - VsOffN = VpVsN`, and the rate of `VsTickN` against `VpDmpT`
is the timer's own (docs/kmd-rm-client.md 15.18.13.3).

A count and its time are two registry values read at slightly different instants, so a
pair can be one tick (4 ms at 240 Hz) apart; this matters for intervals of a few ticks, not
for seconds.

## Owner death and killed processes

Status: implemented on `kmd/scanout-kill-lapse`, never built or run (the KMD cannot be compiled
where it was written); the pure rules are host-tested (`kmd_logic::foreign_scanout`,
`windowed_ready`, `slice_budget`). Origin: a 320.1 desktop stall after a scanout app and then a
Venus app were killed by `TerminateProcess` (ranked findings below).

### What a killed process leaves, and what ends the source

`TerminateProcess` runs no code of the app. Everything the KMD sees of it is what dxgkrnl does
on its behalf afterwards: it destroys the process's contexts and devices (`DxgkDdiDestroyContext`,
`DxgkDdiDestroyDevice`; the order and the delay are dxgkrnl's, and a context with queued GPU work
can delay them), and at the very end `DxgkDdiDestroyProcess`. There is no process-exit callback
into the KMD and none is registered. The foreign source is keyed by the owner token (the
`DeviceContext` pointer, `DeviceOwner::raw()`), so the device is the hook.

| path | who ends the source | counters | when |
|---|---|---|---|
| `SCANOUT_RELEASE` | the owner | `FsRel`, `FsEndBy`=1 | the client's own exit path |
| `DestroyDevice` entry (`foreign_scanout_owner_exit`, `device.rs`) | the KMD | `FsEnd`, `FsXitEnd`, `FsEndBy`=5 | FIRST thing in the DDI, before the mapping drain, the blob and context sweeps |
| `close_all_for_owner` (`foreign_scanout_release_owner`) | the KMD | `FsEnd`, `FsEndBy`=6 | later in the same DDI; finds nothing if the entry hook ran (idempotent) |
| forwarded `Close` of the source's DRM file, transport sweep | the KMD | `FsEnd`, `FsEndBy`=7 | |
| suppression gate: handle no longer the owner's, or another epoch | the KMD | `FsEnd`, `FsEndBy`=8 | every suppressed refresh, so a missed hook cannot wedge it |
| lapse, HPD worker (`foreign_scanout_service`) | the worker's timed wait | `FsLapse`, `FsEndBy`=2 | `lapse_ms` after the last accepted flip (default 2 s) |
| lapse, DISPATCH watchdog (`foreign_scanout_tick`, the vsync tick) | the tick | `FsLapse`, `FsDpcLps`, `FsEndBy`=3 | 250 ms after the deadline, only if the worker did not poll it |
| the owner's next `SCANOUT_PRESENT` found it lapsed | the KMD | `FsLapse`, `FsEndBy`=4 | |
| another owner's `SCANOUT_SET` after the lapse | the KMD | `FsTake`, `FsEndBy`=10 | |
| transport reset / `StopDevice` | the KMD | `FsEnd`, `FsEndBy`=9 | |

Why the entry hook: `close_all_for_owner` used to be the only place a dead owner's source ended
by teardown, and it runs after the mapping drain, the diag dumps, `purge_present_streams`,
`release_blobs_for_owner` and `destroy_contexts_for_owner`. Those are host round trips of up to
30 s each (`SYNC_ROUNDTRIP_TIMEOUT_MS`), per blob and per context, under the scanout and Venus
mutexes. The 2 s lapse still covered the desktop (suppression is a pure function of time, see
below), but the restore flush, the drop of the fenced queue (and the `Close` of its fence
handles), the book release and the next `SET` all waited behind the sweeps. A device without
blobs or contexts (an NVK-on-RM process) loses nothing by the order; a Venus device does.

Suppression cannot outlive the lapse by construction: `suppress_desktop(now)` is
`now < deadline`, a pure read, and the refresh gate and `foreign_scanout_blocks_flip` both ask
it with the current time. A source nobody polled therefore stops suppressing at its deadline
whatever the worker is doing. What a stuck worker cannot do is run the restore (a worker task),
and the state stays `Active` until something polls it, so the counters keep counting it live.
The watchdog's counters (`FsLapse`, `FsDpcLps`, `FsEndBy`, `FsEndT`) cannot be written to the registry at DISPATCH, so
the tick sets a publish-due flag and the next PASSIVE caller mirrors the block: the HPD worker's service pass,
or the escape thread's stuck-only publish (`stall_diag::publish_from_escape`) while the worker looks stuck.
The DISPATCH watchdog closes that second half: the vsync tick (free running, independent of the
worker and of every mutex the worker waits on) checks one atomic (`FS_WATCH_AT`, the live user
source's deadline plus 250 ms; 0 with no source, so a load per tick by default), and past it ends
the source exactly as the worker's lapse does, with the restore request (atomics and `KeSetEvent`,
legal at DISPATCH). The 250 ms grace is the whole reason a healthy driver never sees it act: the
worker's own timed wait expires AT the deadline. `FsDpcLps` > 0 therefore means "the worker was
not looping on time", by itself a finding.

The state machine, read from the counters: `FsSet - FsRel - FsLapse - FsEnd - FsTake` = live user
sources (0 or 1); `FsRest` = `FsRel + FsLapse + FsEnd` once every restore ran; `FsDpcLps <=
FsLapse` and `FsXitEnd <= FsEnd`. `RelTrack = FsFQue + FsFFull` (a minted flip is queued or
refused as full) and `RelGone = FsFFull + FsFSkip (+ whatever left the book first)`.

### Breadcrumbs (`publish_counters`, PASSIVE; times are interrupt-time ms mod 2^32, the clock of
`VpDmpT` and `uptime_ms`)

| value | meaning |
|---|---|
| `FsLive` | 1 a user source holds scanout 0, 2 the KMD's resident one, 0 none |
| `FsOwner`, `FsGen` | low 32 bits of the owner token (the `DeviceContext` pointer), generation of the live source |
| `FsDeadl` | its lapse deadline; older than `FsPubT` on a live source means a lapse nobody polled |
| `FsLastP` | last flip the host took for any user source |
| `FsEndBy`, `FsEndGen`, `FsEndT` | `EndCause` code (table above), generation and time of the LAST end |
| `FsPubT` | time of this publication: every `Fs*`/`Rel*`/`Nv*` value is a mirror written at an edge, so compare it with the uptime before reading anything as "unchanged" |
| `FsDpcLps`, `FsXitEnd` | the two new end paths |
| `WbStaleRdy`, `BlbAbandoned`, `VnRingWd`, `VnRingSl`, `VnRingRt` | the Venus-side findings below |

Publication edges: SET, RELEASE, the worker's lapse, every end by device teardown (new: it
used to leave `FsEnd` unpublished until some later edge; an end by a closed file or the suppression
gate still waits for the next edge), the worker's `Nv*`
mirror, StopDevice. A tester dump taken minutes after the last edge shows the picture at the
edge. `FsPubT` against `uptime_ms` is how old it is.

### Findings of the 320.1 stall, ranked (hardware: not proven)

Evidence: stall after a killed scanout app and then a killed 32-bit Venus app (windowed present
through the UMD's scanout-snapshot, 5152x1440, about 104 ms per frame, 'unsupported source format
65', device creation failed 3 times before). Plain kills (an NVK `d3d11_spin` with RM fences and
KMD flips, a Venus `d3d11_spin`) do NOT reproduce it: the owner-death cleanup of the foreign
source works for those (`FsEnd` +1 and `FsRest` +1 within 2 s).

What the first dump pair cannot show: every `Fs*`, `Rel*`, `Vp*`, `Nv*` value in it is a mirror
last written at about 11:26:34 (`VpDmpT`), 7.4 minutes before the dump, so "unchanged over 10 s"
is meaningless, and the arithmetic at that publish (`FsSet 5 - FsRel 2 - FsLapse 2 - FsEnd 1 = 0`,
`FsRest 5 = 2+2+1`) says no source was live and nothing was owed then. `FsFFull 685` is the
NVK run's: only `SCANOUT_PRESENT` escapes enqueue in the S4 queue (depth 8, no KMD-side wait: a
full queue answers `QUEUE_FULL` at once and the client retries; 685 of 108 739 flips), and a
Venus present cannot touch it. The ~104 ms per frame is not the S4 queue.

1. **The WindowedBlt ready queue holds a dead token (a real defect, fixed here).** An admitted,
   undispatched windowed blt is retired by `terminal_windowed_blt` when the teardown of its
   snapshot resource calls `cancel_windowed_blt_for_resource` (killed process, ring slots
   destroyed). That removed the request from `pending` but left its token at the front of
   `windowed_blt.ready`; `take_ready_windowed_blt` then answered "nothing to dispatch" without
   popping, for every later request of every process, for the rest of the boot. Each such
   present's WDDM fence waits for a blt terminal that is never produced; only the `WddmHeadMs`
   (250 ms) rebase moved the adapter-global FIFO, by cancelling the copy. A large-frame windowed
   app that is slow (one dispatch per worker wake, a 29.7 MB mirror per frame) builds exactly the
   backlog of admitted-undispatched requests this needs; a 64-bit app at 8000 fps does not.
   Fix: the terminal drops the token, the dispatcher pops dead and dispatched front tokens
   (`WbStaleRdy`, must read 0; nonzero = the healing fired).
2. **A "30 s" ring wait that is up to 468 s, under the mutexes the HPD worker waits on without a
   timeout (a real defect, bounded here).** `VenusRing::ring_wait_until` counted one millisecond
   per `sleep_ms(1)`, which sleeps a timer quantum (about 15.6 ms): `RING_WAIT_TIMEOUT_MS` 30 000
   slices is up to about 7.8 minutes of real time. It is reached from `DxgkDdiPresent`
   (`prepare_present_blt`) and from the blob teardown (`release_present_blits_for_resource`)
   holding the scanout and Venus mutexes; the worker takes both (`service_windowed_blt`, the
   refresh, the deferred `SetVidPnSourceAddress`) so its dump counter stops and DWM's flips stop
   for as long as the host does not advance the ring head. 7.4 minutes of stale mirror at the
   dump, still stalled, against a 7.8 minute ceiling, and the device restart that followed, fit.
   What makes the host stop consuming the ring is NOT determined (a dead process's Venus context
   teardown on the host is the suspect). Fix: the real clock bounds the wait as well
   (`slice_budget`): 30 s of real time, then the existing fatal latch, with `VnRingWd` now the larger of
   the real elapsed time and the slice count, `VnRingSl` the slice count alone and `VnRingRt` = 1 when
   the real time reached the budget first (which is nearly always: real time is a little ahead of the
   count even with an exact 1 ms timer, so `VnRingRt` proves nothing; `VnRingSl` far below 30 000 does). Cost: a host that
   stalls the ring for 30 s to 7 minutes and then recovers used to come back and now latches the
   ring fatal (a device restart brings it back); healthy waits (milliseconds) are unchanged.
3. **A blob sweep that stops at the first ambiguity (fixed).** `release_blobs_for_owner_within`
   returned at the first blob whose blit release failed or whose windowed blt was still in flight,
   leaving the owner's remaining blobs in the table with a dead owner token and their pending
   windowed blts (each holding a read-ledger ticket and one of 64 token slots) un-cancelled.
   Killed apps fill the blob table, the token slots and the ledger a few at a time. Now the rest
   are drained without host commands, their undispatched windowed blts cancelled and their
   read-ledger claims ended (the 8-slot ledger would otherwise run out and every windowed
   present would get `STATUS_NO_MEMORY`; a claim with a reader still active is pinned until that
   reader's ticket retires, then reclaimed) (`BlbAbandoned`,
   must read 0).
4. **Closing present-stream slots keep undispatched requests alive (fixed).** A purge that finds a
   mid-frame stream only marks it closing; the sweep that cancels the requests of dead streams
   ran at that moment, when the slot still counted as live, and was not repeated when the
   context's `CTX_DESTROY` finalized the slot. It is now repeated there, together with
   `discharge_dead_present_stream_waits` (the callers hold the notification-ordered token already, and
   finalize only runs after a successful `CTX_DESTROY`, i.e. an explicit cancellation, which is the case
   that sweep is defined for).
5. Not changed, named for the next look: a windowed blt destination buffer left in `KmdWriter` /
   `KmdCpuMirror` after a rejected blt or an early return in the legacy Present arm
   (`present_buffer` ownership never returns, `try_begin_present_buffer_write` stays Busy and the
   allocation cannot be destroyed); an unfulfilled wire fence at the head of the adapter-global
   FIFO (the wire arm cannot be rebased); the FIFO overflow latch (`failed`, never cleared).

The 104 ms per frame constant is not a timer: no 100 ms or 104 ms wait exists in the KMD. At
5152x1440x4 (29.7 MB) the windowed blt costs a ring copy plus a CPU mirror of the whole
destination on the single HPD worker, one request per wake, under the scanout mutex; 25 vsync
periods at 240 Hz happens to be 104.17 ms but nothing counts them.

### Hardware checklist

1. Read `FsPubT` against `uptime_ms` first. If the stall is live and `FsPubT` is minutes old,
   nothing below can be concluded from the other mirrors.
2. After a deliberate kill of a scanout app: `FsEnd` +1, `FsXitEnd` +1, `FsEndBy` 5, `FsEndT`
   within about the DDI's delay of the kill, `FsRest` +1, `FsLive` 0. `FsEndBy` 6 means the entry
   hook did not run first (check `DestroyDevice`); 2 or 3 means DestroyDevice never ran for that
   device, and `FsDpcLps` > 0 means the HPD worker was late (read the worker breadcrumbs of the
   `kmd/stall-watchdog` lane).
3. After a kill of a Venus windowed app that was slow and queued (Heaven at 5152x1440): `WbStaleRdy`
   (a healed wedge: expected 0 or small once, never growing), `BlbAbandoned` (0 unless a blit
   was in flight), `VnRingWd` / `VnRingSl` / `VnRingRt` (present only if a ring wait
   expired; `VnRingSl` of about 2 000 against a budget of 30 000 with `VnRingWd` about 30 000 proves the
   timer-quantum mismatch; `VnRingRt` alone does not).
4. DWM present counts before and after a deliberate kill and a notepad window, as in
   `killrepro.sh`; a stall with `VnRingWd` absent for under 30 s of real time points at the ring wait
   (the latch fires at 30 s), a stall with `WfBBlt` rising points at item 1.
5. Not to be inferred from a dump: that a source is live. `FsLive` and `FsDeadl` say so.

## What is not done

* The flip is not issued from the WDDM present/flip path. `PRESENT` is a PASSIVE
  escape and sends the host flip synchronously (its round trip is the backpressure).
  Tying it to WDDM would mean the app's WDDM present of a placeholder surface arming
  a worker-side host flip paced by the vsync timer, completing the DMA fence from
  the flip's reply. `vsync_count` and the pending-vidpn machinery already exist for
  that; it needs a hardware run to get the ordering right and was not attempted
  blind.
* Without the host's release feature (see "Buffer release") there is no release event to the
  app: it must not reuse a GEM image before the next `PRESENT` has returned (N-buffer rotation,
  as the smoke test does). With it, `SCANOUT_STATUS` says exactly when.
* One `PRESENT` at a time per device is the client's job; the KMD mints `seq` in call
  order but the host takes frames in arrival order.
* Desktop cursor and `ScanoutDisable` paths are not touched.
* `HELIOS_NVRM_ST_SCANOUT_BUSY = 13` and `NO_SOURCE = 14` were picked as the next free
  `HELIOS_NVRM_ST_*`; renumber on merge if another branch took them.

## Untested

Everything in `kmd_render`. Host tests: `cargo test` in `guest/windows/kmd_logic`
(15 tests for the state machine; copy the crate out of the repo workspace first, it
resolves the repo-root workspace otherwise). Layout asserts: `protocol` compiles
(Rust `const` asserts) and the header passes `gcc -m32/-m64 -fsyntax-only`.
