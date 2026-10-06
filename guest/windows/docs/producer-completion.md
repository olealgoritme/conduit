# Allocation producer completion table

`kmd_logic/src/producer_completion.rs` (logic, host-tested) and
`kmd_render/src/adapter/producer.rs` (the lock, the status page, the counters). Escape
`HELIOS_ESCAPE_PRODUCER` (0x13, 96 bytes): `MAP`, `BIND`, `PUBLISH`, `WAIT`, `CANCEL`,
`RELEASE`, `ABORT` (`protocol/src/producer.rs`).

## Model

* One status slot per dxgkrnl allocation (`HELIOS_PRODUCER_SLOTS` = 8192): `announced`
  (epochs published) and `completed` (a prefix of them), seqlock-mirrored to the page `MAP`
  returns.
* `PUBLISH(stream, value)` announces the next epoch of the allocation and creates a
  **pending entry** `(epoch, stream, value)` linked at the tail of that allocation's list.
  Table sizes: 8192 pending entries, 16384 writer slots, 64 stream marks.
* An allocation's `completed` advances only through the **linked prefix**: entry N+1 can be
  `done` and still occupy its pending slot until entry N is done (a consumer must never see
  epoch N+1 before N). So one entry that never completes holds every later entry of its
  allocation in the table.
* A **writer slot** is the last published value of one (allocation, stream) pair; it makes
  `value` strictly increasing per pair while entries of the pair are in flight. A **mark**
  is the highest completed value of one live stream.
* `value == 0` publishes an already-complete epoch (see `zero-copy-present.md` 10.4); it holds
  no writer and takes a pending slot only when queued behind an older pending epoch.

## Where completions come from

`virtio/gpu/mod.rs`: a tagged Venus submit carries `(stream, value)` from the ICD's signal
batch (`vn_signal_win32_external_semaphore`). When its used-ring response is OK,
`retire_present_stream_value` -> `Progress::wire` raises the stream's `retired` to
`max(retired, value)` -> `complete_present_stream_gpu` calls
`ProducerCompletion::complete(stream, progress.completed())`. That argument is the stream's
**cumulative** completed value, not the value of the submission that returned. It is called
under the virtio lock; `publish_producer` also runs under the virtio lock and passes
`already_complete = progress.completed() >= value`, so a retirement that beat the publish is
seen by the publish (that race was already closed by the caller).

A stream dies through `close_present_stream_slot` -> `ProducerCompletion::fail_stream`
(unregister, device/context purge, a rejected tagged submit) or `purge_all_present_streams`
-> `reset`. `fail_stream` fails (never completes) every allocation with an entry on that
stream.

## The completion rule (the fix)

Stream values are positions on a timeline semaphore: signalled in order, so a completed value
proves every earlier one. `Table::complete(stream, C)` therefore completes **every pending
entry of `stream` with `value <= C`** (serial-number comparison `reached`, RFC 1982: correct
across a u32 wrap inside half the value space; `0` is the "already complete" marker, never a
position, and `complete(_, 0)` does nothing).

The old rule was `value == C`. It stranded an entry whenever its own value was never the
reported one: values that reach the host out of order (the KMD's `Progress` keeps the
maximum, so wire(12) then wire(9) reports 12 twice), a signal batch the ICD did not tag (no
`ring_seqno_valid`: legacy), a tag the KMD refuses as stale (`value <= submitted_value`), and in
general any value whose own submission is not the one reported. The stranded entry blocked its
allocation's prefix, every later entry of that allocation queued behind it (done, slot still
held), and after 8192 of them `PUBLISH` returned `STATUS_INSUFFICIENT_RESOURCES`
(`Error::Capacity`), the UMD logged `producer: allocation epoch publication failed`, aborted
the allocation and the present was refused.

## Early completion

The table remembers per live stream the highest completed value (`Mark`). `publish` completes
the new entry at once when `value <= watermark`, or when the caller's proof
(`already_complete`) says so, and raises the watermark to a proven value. A complete entry
with nothing older pending on its allocation takes **no** pending slot and no writer slot; behind
an older pending epoch it queues done (prefix rule) with a pending slot but no writer. The
re-check is inside `publish`, under the same lock as `complete`. If every mark slot is taken
(`PrdMarkFull`) the stream degrades to the caller's proof alone, never to a refusal.
A report for a stream the table has not published on is ignored (no mark is created from a
completion, so a stream that already died cannot leak one).

## Slot lifetime

* Pending slot: freed when the entry reaches the head of its allocation `done`, or when its
  allocation fails/is removed/reset.
* Writer slot: freed when the **last pending entry of its pair leaves** (counted per slot), or
  with its stream (`fail_stream`), its allocation (`remove`), or `reset`. Before this change
  they were freed only by the latter three, so they were bounded by allocations x streams, not
  leaked, but idle pairs stayed. Consequence: once a pair is idle the strictly-increasing check
  is gone; a replay of a value is then either at or below the stream's watermark (complete on
  arrival, harmless) or above it (a new in-flight value).
* Mark: freed with its stream.
* Dead streams are **discharged by failing**, never by completing: `fail_stream` marks each
  affected allocation `FAILED`, drops all its entries (including entries of other streams) and
  releases their pending/writer slots and the stream's mark. No host work is assumed complete.

## STATUS_INSUFFICIENT_RESOURCES sites (`Error::Capacity` -> 0xC000009A)

| site | meaning |
|---|---|
| `Table::publish` / `publish_complete`: no free pending slot | `PrdFull` |
| `Table::publish`: no free writer slot | `PrdWrFull` (cannot happen while pending <= writers: every writer has an entry) |
| `Table::publish`: epoch counter overflow | never in practice |
| `ProducerCompletion::*`: table not initialised | `StartDevice` failed `producer.init()` (`PrInitF`) |
| `bind`: 16384 bindings, binding counter, retain overflow | not the publish path |
| `wait`: 1024 waiters | |
| `MAP`: mapping failed / per-device mapping table full | |

Stream-slot exhaustion is a different error (`register_present_stream` -> device error); a
publish naming no live stream is `STATUS_INVALID_PARAMETER`, not 0xC000009A.

## Counters (registry, written by `publish_nvrm_counters`, and on every 64th refused escape)

`PrdPend` entries in the table now (should track frames in flight; a value that only rises is a
completion that never arrives), `PrdHi` its high-water mark, `PrdWr` / `PrdWrHi` writer slots now /
high-water, `PrdMarks` marks in use, `PrdFull` publishes refused for a full pending table,
`PrdWrFull` for a full writer table, `PrdMarkFull` streams that found no mark slot, `PrdSkip`
entries completed by a later value than their own (the fix firing: nonzero on a session that
used to leak), `PrdEarly` publishes completed on arrival by the table's own watermark.
Existing: `PrPub` / `PrRet` published / completion calls, `PrRef` refused escapes.

## Not done on purpose

No age-based retirement. An entry whose host work may not have completed must never be
retired (it would let a consumer read an unfinished surface), and nothing the table sees can
prove that for a stream that is still alive. Entries of dead streams are failed (above);
`PrdPend` / `PrdHi` make a remaining leak visible instead of hiding it.

## Tests

`kmd_logic/src/producer_completion.rs` (host `cargo test`): exact value, skipped value (3
completes 1..3), a skipped head no longer holding later entries, out-of-order reports,
completion before publication (table watermark and caller proof), early completion queued
behind an older epoch, writer reuse and exhaustion, pending exhaustion and recovery, a field
repro (one never-reported value then 10 000 exact ones in a 32-slot table), a 100 000-entry
soak with batched/late/early reports that never exceeds a bound, u32 wrap, dead-stream
discharge, stream churn, mark exhaustion, allocation removal/reset, and a randomized check that
the incremental counters equal a full scan. `kmd_render` changes were type-checked only against
a stub harness (module visibility mirrored); nothing here has run on a Windows guest.
