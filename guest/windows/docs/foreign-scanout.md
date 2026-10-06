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
* device teardown (`close_all_for_owner`, called from DestroyDevice);
* the suppression gate finding the handle no longer the owner's, or the NVRM epoch
  changed (re-checked on every suppressed refresh, so a missed hook cannot wedge it);
* the lapse: no `PRESENT` for `lapse_ms` (default 2 s, 100 ms..30 s). The HPD worker
  waits with a timeout equal to the remaining lapse while a source is live, so a
  silent owner gives the desktop back with no other edge.
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
the live-source arithmetic of the counters below stays a count of user sources.

A forwarded `ScanoutFlip` (FORWARD) from a device that does not hold a live source is
refused `FORBIDDEN`; with no source live FORWARD flips behave as before (and race the
desktop, as before).

## Counters (`publish_nvrm_counters`, also on SET/RELEASE/lapse)

`FsSet FsPres FsRel FsLapse FsEnd FsTake FsSupp FsRest FsRef FsErr`. Live sources =
`FsSet - FsRel - FsLapse - FsEnd - FsTake` (0 or 1). `FsSupp` rising with no source
live is a bug. `FsRest` should track `FsRel + FsLapse + FsEnd`.

## What is not done

* The flip is not issued from the WDDM present/flip path. `PRESENT` is a PASSIVE
  escape and sends the host flip synchronously (its round trip is the backpressure).
  Tying it to WDDM would mean the app's WDDM present of a placeholder surface arming
  a worker-side host flip paced by the vsync timer, completing the DMA fence from
  the flip's reply. `vsync_count` and the pending-vidpn machinery already exist for
  that; it needs a hardware run to get the ordering right and was not attempted
  blind.
* No release event to the app: the host never sends one, so the app must not reuse a
  GEM image before the next `PRESENT` has returned (N-buffer rotation, as the smoke
  test does).
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
