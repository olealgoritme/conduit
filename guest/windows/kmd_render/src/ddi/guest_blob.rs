//! Guest-memory blob as the Blt copy destination (`GuestBlob`): the I/O half. The rules are
//! `helios_kmd_logic::guest_blob` (host-tested; every host-facing constant is in its
//! `contract` module, the config bit is `helios_protocol::NVGPU_CFG_GUEST_BLOB`); the Venus
//! objects are `virtio/venus/guest_blob.rs`. Design, lifecycle, failure matrix and what is not
//! verified: `docs/zero-copy-present.md` section 24.12.
//!
//! Knob (REG_DWORD in the service key, default 0 = the previous behaviour; read at every
//! StartDevice by [`reset_for_start`] and once on first use): `GuestBlob` 1 lets a Blt into a
//! KMD standard buffer whose system backing is fully leased write those pages directly.
//!
//! Counters (at most 14 characters, the list is `helios_kmd_logic::guest_blob::COUNTERS`;
//! atomics, written to the registry by [`publish_counters`] from `publish_nvrm_counters`):
//! `GbKnob` / `GbFeat` (knob in force, host advertises the feature), `GbMade` (guest blobs
//! created and imported), `GbHit` (Present copies submitted into one), `GbDrop` (teardowns),
//! `GbDrainUs` / `GbDrainMax` (drain time total / longest), `GbFail` (failures = strikes),
//! `GbWhy` / `GbMask` (last reason / every reason, `guest_blob::Why`), `GbRefuse` (Presents a
//! decision kept on the legacy copy), `GbStrike` (destinations disabled), `GbLeak`
//! (destinations whose pages stay pinned after a failed release), `GbRuns` / `GbBytes` (the
//! last create), `GbLive` / `GbLiveRuns` (live blobs and runs), `GbLost` (deferred copies whose
//! guest blob was retired before submission, re-prepared into the destination's current target).

use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::guest_blob::deadline::{self, Limits};
use helios_kmd_logic::guest_blob::{self as gb, contract as gbc, Drain, Eligible, Why};

use crate::adapter::{AdapterContext, SystemBackingGuard};
use crate::ddi::escape_wait::begin_bounded;
use crate::irql::PassiveLevel;
use crate::virtio::ctrl::{self, GuestBlobCreateError};

const UNREAD: u32 = u32::MAX;
static KNOB: AtomicU32 = AtomicU32::new(UNREAD);

static FEAT: AtomicU32 = AtomicU32::new(0);
static MADE: AtomicU32 = AtomicU32::new(0);
static HIT: AtomicU32 = AtomicU32::new(0);
static DROP: AtomicU32 = AtomicU32::new(0);
static DRAIN_US: AtomicU32 = AtomicU32::new(0);
static DRAIN_MAX: AtomicU32 = AtomicU32::new(0);
static FAIL: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static MASK: AtomicU32 = AtomicU32::new(0);
static REFUSE: AtomicU32 = AtomicU32::new(0);
static STRIKE: AtomicU32 = AtomicU32::new(0);
static LEAK: AtomicU32 = AtomicU32::new(0);
static RUNS: AtomicU32 = AtomicU32::new(0);
static BYTES: AtomicU32 = AtomicU32::new(0);
static LIVE: AtomicU32 = AtomicU32::new(0);
static LIVE_RUNS: AtomicU32 = AtomicU32::new(0);
static LOST: AtomicU32 = AtomicU32::new(0);

#[inline(never)]
fn read_knob() -> bool {
    let v = crate::diag::read_config_dword(crate::diag::knobs::GUEST_BLOB, 0) != 0;
    KNOB.store(v as u32, Ordering::Relaxed);
    v
}

/// `GuestBlob` is on. One relaxed load once read; the first read is PASSIVE (registry).
pub(crate) fn knob_on() -> bool {
    match KNOB.load(Ordering::Relaxed) {
        UNREAD => read_knob(),
        v => v != 0,
    }
}

/// A new transport generation: the knob is read again and mirrored with the value in force
/// (0 included), and the counters are zeroed. PASSIVE.
pub(crate) fn reset_for_start() {
    for cell in [
        &FEAT, &MADE, &HIT, &DROP, &DRAIN_US, &DRAIN_MAX, &FAIL, &WHY, &MASK, &REFUSE, &STRIKE,
        &LEAK, &RUNS, &BYTES, &LIVE, &LIVE_RUNS, &LOST,
    ] {
        cell.store(0, Ordering::Relaxed);
    }
    let on = read_knob();
    crate::diag::record_named_bytes(b"GbKnob", on as u32);
}

/// Mirror the counters to the service key once anything happened. PASSIVE only.
pub(crate) fn publish_counters() {
    let events = MADE.load(Ordering::Relaxed)
        | FAIL.load(Ordering::Relaxed)
        | REFUSE.load(Ordering::Relaxed)
        | DROP.load(Ordering::Relaxed);
    if events == 0 {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"GbKnob", KNOB.load(Ordering::Relaxed) & 1);
    rec(b"GbFeat", FEAT.load(Ordering::Relaxed));
    rec(b"GbMade", MADE.load(Ordering::Relaxed));
    rec(b"GbHit", HIT.load(Ordering::Relaxed));
    rec(b"GbDrop", DROP.load(Ordering::Relaxed));
    rec(b"GbDrainUs", DRAIN_US.load(Ordering::Relaxed));
    rec(b"GbDrainMax", DRAIN_MAX.load(Ordering::Relaxed));
    rec(b"GbFail", FAIL.load(Ordering::Relaxed));
    rec(b"GbWhy", WHY.load(Ordering::Relaxed));
    rec(b"GbMask", MASK.load(Ordering::Relaxed));
    rec(b"GbRefuse", REFUSE.load(Ordering::Relaxed));
    rec(b"GbStrike", STRIKE.load(Ordering::Relaxed));
    rec(b"GbLeak", LEAK.load(Ordering::Relaxed));
    rec(b"GbRuns", RUNS.load(Ordering::Relaxed));
    rec(b"GbBytes", BYTES.load(Ordering::Relaxed));
    rec(b"GbLive", LIVE.load(Ordering::Relaxed));
    rec(b"GbLiveRuns", LIVE_RUNS.load(Ordering::Relaxed));
    rec(b"GbLost", LOST.load(Ordering::Relaxed));
}

// ---- counters, callable at any IRQL (atomics only) -----------------------------------------

/// A decision kept this Present on the legacy copy (no strike).
pub(crate) fn note_refused(why: Why) {
    REFUSE.fetch_add(1, Ordering::Relaxed);
    WHY.store(why.code(), Ordering::Relaxed);
    MASK.fetch_or(why.bit(), Ordering::Relaxed);
}

/// A failure (a strike). `disabled`: it was the destination's last.
pub(crate) fn note_failed(why: Why, disabled: bool) {
    FAIL.fetch_add(1, Ordering::Relaxed);
    WHY.store(why.code(), Ordering::Relaxed);
    MASK.fetch_or(why.bit(), Ordering::Relaxed);
    if disabled {
        STRIKE.fetch_add(1, Ordering::Relaxed);
    }
    if why.poisons() {
        LEAK.fetch_add(1, Ordering::Relaxed);
    }
}

/// One Present copy was submitted into a guest blob.
pub(crate) fn note_hit() {
    HIT.fetch_add(1, Ordering::Relaxed);
}

/// A deferred copy prepared for a guest blob found it retired at submission and was prepared
/// again into the destination's current target (`VenusClient::retarget_prepared_present_blt`).
/// The name is historical: the frame is no longer lost.
pub(crate) fn note_lost() {
    LOST.fetch_add(1, Ordering::Relaxed);
}

/// The host advertises the feature (recorded once per generation, at first use).
pub(crate) fn note_feature(advertised: bool) {
    FEAT.store(advertised as u32, Ordering::Relaxed);
}

// ---- the I/O ---------------------------------------------------------------------------------

/// Whether the host serves guest blobs: `NVGPU_CFG_GUEST_BLOB` together with `NVGPU_CFG_VENUS`
/// in the device config `features` word, read where the other device feature bits are.
fn advertised(adapter: &AdapterContext) -> bool {
    adapter
        .with_virtio(|v| {
            gbc::advertised(
                v.nvrm_device_features(),
                helios_protocol::NVGPU_CFG_GUEST_BLOB,
            )
        })
        .unwrap_or(false)
}

fn mirror_live(adapter: &AdapterContext) {
    let (blobs, runs) = adapter.system_backings.guest_live();
    LIVE.store(blobs, Ordering::Relaxed);
    LIVE_RUNS.store(runs, Ordering::Relaxed);
}

/// The Blt arm, before any copy path, for a KMD standard-buffer destination (`resource_id`,
/// `pitch` x `height`, `allocation_size` bytes) presented by the process `presenter`: make
/// sure the destination's guest blob exists when it should (lazily, on the first Blt that finds
/// the destination's system backing fully leased: the import costs a few milliseconds once per
/// destination) and is retired when it must not be used. The copy itself decides under the
/// Venus mutex whether it goes into a guest buffer (`VenusClient::guest_target_for`).
///
/// One relaxed load with the knob at 0. PASSIVE, no lock held (it takes the content
/// transaction, then the Venus mutex, then the virtio lock: the paging path's order).
#[inline(never)]
pub(crate) fn prepare(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    pitch: u32,
    height: u32,
    allocation_size: u64,
    presenter: usize,
) {
    if !knob_on() {
        return;
    }
    let advertised = advertised(adapter);
    note_feature(advertised);
    let foreign_consumer = adapter
        .with_virtio(|v| v.present_buffer_foreign_open(resource_id, presenter))
        .unwrap_or(true);
    let decision = gb::eligible(gb::Facts {
        knob_on: true,
        advertised,
        dst_standard_buffer: true,
        foreign_consumer,
        system_copy_invalid: adapter.system_backings.system_copy_invalid(resource_id),
        record: adapter.system_backings.guest_record(resource_id),
    });
    match decision {
        Eligible::Use => {}
        Eligible::No(why) => note_refused(why),
        Eligible::Retire(why) => {
            note_refused(why);
            if let Some(guard) = adapter.system_backings.serialize(passive) {
                retire(passive, adapter, &guard, resource_id, Limits::NORMAL);
            }
        }
        Eligible::Create => {
            // The common miss (the destination is in the BAR segment: no leases at all) costs
            // one spinlock, not the content transaction.
            if !adapter.system_backings.is_backed(resource_id) {
                note_refused(Why::Uncovered);
                return;
            }
            create(
                passive,
                adapter,
                resource_id,
                pitch,
                height,
                allocation_size,
            );
        }
    }
}

/// A create or import failed after the record went to `Creating` and the budget was charged
/// `runs`, and nothing is left on the host: refund, strike, count.
fn fail_create(guard: &SystemBackingGuard<'_>, resource_id: u32, why: Why, runs: u32) {
    let disabled = guard
        .guest_update(resource_id, false, |record, budget| {
            budget.refund(runs);
            record.create_failed(why);
            record.disabled()
        })
        .unwrap_or(false);
    note_failed(why, disabled);
}

/// Create the guest blob over the destination's leases and import it. Under the content
/// transaction from the lease snapshot to the end, so no lease can change in between.
#[inline(never)]
fn create(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    resource_id: u32,
    pitch: u32,
    height: u32,
    allocation_size: u64,
) {
    let Some(guard) = adapter.system_backings.serialize(passive) else {
        note_refused(Why::Busy);
        return;
    };
    // A marked system copy (a skipped eviction, `BltNoMirror`): its page-in will be skipped in
    // favour of the Venus blob, so the pages must not become the newest copy. Checked again
    // under the transaction (the decision in `prepare` read it without one).
    if guard.system_copy_invalid(resource_id) {
        return note_refused(Why::SystemStale);
    }
    let cover = match gb::cover_len(pitch, height, allocation_size) {
        Ok(cover) => cover,
        Err(why) => return note_refused(why),
    };
    let Some(snapshot) = guard.snapshot(resource_id) else {
        return note_refused(Why::Uncovered);
    };
    let mut pieces = Vec::new();
    if !snapshot.pieces(&mut pieces) {
        return note_refused(Why::Kmd);
    }
    let runs = match gb::build_runs(&pieces, cover, |_| {}) {
        Ok(runs) => runs,
        Err(why) => return note_refused(why),
    };
    // Admit (the host's totals), claim (None/Gone -> Creating) and charge in one critical
    // section: two creators cannot both take the last slot.
    let claimed = guard.guest_update(resource_id, true, |record, budget| {
        budget.admits(runs)?;
        record.begin_create()?;
        budget.charge(runs)
    });
    match claimed {
        Some(Ok(())) => {}
        Some(Err(why)) => return note_refused(why),
        None => return note_refused(Why::Kmd),
    }
    // `runs` <= MAX_ENTRIES (build_runs refuses more).
    let runs32 = runs as u32;
    let mut entries: Vec<u8> = Vec::new();
    if entries.try_reserve_exact(runs * gbc::ENTRY_BYTES).is_err() {
        return fail_create(&guard, resource_id, Why::Kmd, runs32);
    }
    let filled = gb::build_runs(&pieces, cover, |run| {
        // `len` <= MAX_RUN_BYTES < 4 GiB; the capacity was reserved, so this cannot allocate.
        entries.extend_from_slice(&gbc::encode_entry(run.addr, run.len as u32));
    });
    if filled != Ok(runs) {
        return fail_create(&guard, resource_id, Why::Kmd, runs32);
    }
    // The pin holds every lease the entries name until the record says they may go.
    let Some(pin) = snapshot.pin() else {
        return fail_create(&guard, resource_id, Why::Kmd, runs32);
    };
    // The round trip, its enqueue retries and the virtio lock: at most `deadline::CREATE_MS`.
    let created = {
        let _bounded = begin_bounded(deadline::CREATE_MS);
        ctrl::create_guest_blob(
            passive,
            adapter,
            adapter.venus_ctx_id(),
            &entries,
            runs32,
            cover,
            u64::from(deadline::CREATE_MS),
        )
    };
    let guest = match created {
        Ok(guest) => guest,
        Err(GuestBlobCreateError::Host { resp_type, errno }) => {
            let why = gbc::classify_create(resp_type, errno);
            return fail_create(&guard, resource_id, why, runs32);
        }
        Err(GuestBlobCreateError::Transport) => {
            return fail_create(&guard, resource_id, Why::Transport, runs32);
        }
        Err(GuestBlobCreateError::NoSlot) => {
            return fail_create(&guard, resource_id, Why::Kmd, runs32);
        }
        Err(GuestBlobCreateError::Unanswered) => {
            // The host may still create the blob over these pages: they stay pinned for the
            // life of this generation (`CreateTimeout` poisons; the budget is not refunded,
            // the host may count it). The record keeps the pin.
            let disabled = guard
                .guest_update(resource_id, false, |record, _| {
                    record.create_failed(Why::CreateTimeout);
                    record.disabled()
                })
                .unwrap_or(false);
            note_failed(Why::CreateTimeout, disabled);
            if let Err(pin) = guard.guest_set_pin(resource_id, pin) {
                // No record to hold it: leak it rather than unlock pages the host may map.
                core::mem::forget(pin);
            }
            mirror_live(adapter);
            return;
        }
    };
    let _ = guard.guest_update(resource_id, false, |record, _| record.sent(guest, runs32));
    RUNS.store(runs32, Ordering::Relaxed);
    BYTES.store(cover.min(u32::MAX as u64) as u32, Ordering::Relaxed);
    // The Venus mutex and every ring command of the import (its unwind included): at most
    // `deadline::IMPORT_MS`. A wait that runs out fails its step; a step whose objects may
    // exist on the host makes the import unclean (the pages then stay pinned).
    let imported = {
        let _bounded = begin_bounded(deadline::IMPORT_MS);
        adapter.with_venus_client(passive, |client| {
            client.import_guest_blob(adapter, resource_id, guest, cover)
        })
    };
    let failure = match imported {
        Ok(Ok(())) => None,
        Ok(Err(crate::virtio::venus::ImportFailed { why, clean })) => Some((why, clean)),
        Err(_) => Some((Why::Transport, true)),
    };
    match failure {
        None => {
            let _ = guard.guest_update(resource_id, false, |record, _| record.created());
            if let Err(pin) = guard.guest_set_pin(resource_id, pin) {
                // Unreachable (the record exists under this transaction). Never unlock pages
                // the host maps: keep this pin alive for ever, then retire the blob.
                core::mem::forget(pin);
                retire(passive, adapter, &guard, resource_id, Limits::NORMAL);
                return;
            }
            MADE.fetch_add(1, Ordering::Relaxed);
            // A mark can be set without the content transaction (a skipped eviction whose
            // mutex failed, a `BltNoMirror` copy): one that arrived during the create retires
            // the new blob before any copy targets it.
            if guard.system_copy_invalid(resource_id) {
                note_refused(Why::SystemStale);
                retire(passive, adapter, &guard, resource_id, Limits::NORMAL);
            }
        }
        Some((why, clean)) => {
            // Everything the import made was released (and fenced): the blob can go, and only
            // after its UNREF answered may the pin (the pages) go.
            if clean && release_blob(passive, adapter, guest, deadline::UNREF_MS).is_ok() {
                fail_create(&guard, resource_id, why, runs32);
                drop(pin);
            } else {
                // The host may still map the pages: they stay pinned for the life of this
                // destination's record (until the transport generation ends).
                let _ = guard.guest_update(resource_id, false, |record, _| {
                    record.create_failed(Why::ReleaseFailed)
                });
                note_failed(why, false);
                note_failed(Why::ReleaseFailed, true);
                if let Err(pin) = guard.guest_set_pin(resource_id, pin) {
                    // No record to hold it: leak it rather than unlock pages the host maps.
                    core::mem::forget(pin);
                }
            }
        }
    }
    mirror_live(adapter);
}

/// Retire `resource_id`'s guest blob, in the order the host requires, BEFORE any of its leases
/// change:
///
/// 1. no new copy targets it (the Venus record is marked retired);
/// 2. drain: bounded waits on the wire fences of the copies into it, then a queue fence;
/// 3. release the cached copy commands;
/// 4. `vkDestroyBuffer` + `vkFreeMemory`;
/// 5. a fence after the free (Venus ring commands are asynchronous to the UNREF);
/// 6. `RESOURCE_UNREF`;
/// 7. only then drop the pin, so the caller's lease change may unlock the pages.
///
/// Returns whether the pages may be unlocked. `false`: a step failed, the pin stays (the pages
/// stay locked whatever the lease change does) and the destination is poisoned. Never fails
/// the caller: `BuildPagingBuffer` must answer success whatever happens here.
///
/// Bounded by `limits` (`helios_kmd_logic::guest_blob::deadline`): steps 1 to 5, the Venus
/// mutex included, run in one bounded section of `limits.drain_ms` (each fence and marker at
/// most `FENCE_MS` of it), step 6 in one of `limits.unref_ms`. A wait that runs out is a
/// strike that poisons (`DrainTimeout` / `ReleaseFailed`): the record stops being a copy
/// target at once (`VenusClient::guest_target_for` reads it), the pages stay pinned until the
/// generation ends, and the thread is back within about two seconds whatever the host does.
///
/// PASSIVE, content transaction held (`guard`): content -> Venus -> virtio.
#[inline(never)]
fn retire(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    guard: &SystemBackingGuard<'_>,
    resource_id: u32,
    limits: Limits,
) -> bool {
    let guest = match guard.guest_update(resource_id, false, |record, _| record.begin_drain()) {
        None | Some(Drain::Nothing) => return true,
        Some(Drain::Poisoned) => return false,
        Some(Drain::Release { guest }) => guest,
    };
    let t0 = crate::ddi::blt_async::now_100ns();
    let released = {
        let _bounded = begin_bounded(limits.drain_ms);
        adapter.with_venus_client(passive, |client| {
            client.retire_guest_buffer(adapter, resource_id, guest)
        })
    };
    let step = match released {
        Ok(result) => result,
        // No Venus client, or its mutex wait ran out: nothing proves the import is gone, so the
        // pages stay pinned. The generation reset that follows a lost client frees them.
        Err(_) => Err(Why::ReleaseFailed),
    };
    let step = step.and_then(|()| {
        release_blob(passive, adapter, guest, limits.unref_ms).map_err(|_| Why::ReleaseFailed)
    });
    let us =
        (crate::ddi::blt_async::now_100ns().saturating_sub(t0) / 10).min(u32::MAX as u64) as u32;
    DRAIN_US.fetch_add(us, Ordering::Relaxed);
    DRAIN_MAX.fetch_max(us, Ordering::Relaxed);
    let ok = match step {
        Ok(()) => {
            let _ = guard.guest_update(resource_id, false, |record, budget| {
                let runs = record.drained();
                budget.refund(runs);
            });
            DROP.fetch_add(1, Ordering::Relaxed);
            // `Gone`: nothing on the host names the pages. PASSIVE, outside the spinlock.
            drop(guard.guest_take_pin(resource_id));
            true
        }
        Err(why) => {
            let _ = guard.guest_update(resource_id, false, |record, _| record.drain_failed(why));
            note_failed(why, true);
            false
        }
    };
    mirror_live(adapter);
    ok
}

/// Before ANY change to `resource_id`'s leases or to the system pages they name (a paging
/// transfer in either direction, a discard): retire its guest blob first. One spinlock lookup
/// when the destination has none (always, with the knob at 0). PASSIVE, content transaction
/// held.
pub(crate) fn before_lease_change(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    guard: &SystemBackingGuard<'_>,
    resource_id: u32,
) {
    let Some(record) = adapter.system_backings.guest_record(resource_id) else {
        return;
    };
    if record.may_unlock() {
        return;
    }
    retire(passive, adapter, guard, resource_id, Limits::NORMAL);
}

/// The guest blob's `RESOURCE_UNREF`, in a bounded section of `limit_ms` (the enqueue retries
/// and the virtio lock included) and with the same round-trip timeout.
fn release_blob(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    guest: u32,
    limit_ms: u32,
) -> Result<(), crate::virtio::VirtioError> {
    let _bounded = begin_bounded(limit_ms);
    ctrl::release_guest_blob_within(passive, adapter, guest, u64::from(limit_ms))
}

/// The transport generation is about to end (StopDevice, or a StartDevice that finds an old
/// transport): while the transport and the Venus client still answer, retire every live guest
/// blob in the host's order (drain, destroy, free, fence, UNREF), so that no copy the host may
/// still run can write a page whose pin the generation reset (`reset_generation`) then drops.
///
/// Bounded by `budget` (the sweep budget of the caller): nothing is sent once it is spent. A
/// blob that is not retired (budget spent, a step failed: poisoned) keeps its pin until the
/// generation reset, which runs only after the transport was reset (`VirtioGpu::drop` sets the
/// device status to 0; a reset device may not access guest memory, and a guest blob's mapping
/// is exactly such an access), so the pages are unlocked only once the host can no longer write
/// them. One spinlock lookup when there is no record (always, with the knob at 0). PASSIVE, no
/// lock held (takes the content transaction, then the Venus mutex: the paging path's order).
pub(crate) fn retire_all_for_stop(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    budget: &helios_kmd_logic::sweep_budget::SweepBudget,
) {
    if !adapter.system_backings.guest_any() {
        return;
    }
    let Some(guard) = adapter.system_backings.serialize(passive) else {
        return;
    };
    // `retire` moves a live record out of `Ready` whatever happens, so each turn retires a
    // different destination; the bound is the table's size.
    for _ in 0..crate::adapter::SystemBackingTable::GUEST_RECORDS {
        let Some(resource_id) = adapter.system_backings.guest_first_ready() else {
            break;
        };
        let now = crate::adapter::foreign_scanout::now_100ns();
        let Some(cap_ms) = budget.call_timeout_ms(now) else {
            // Spent: send nothing more. The pins stay until the generation reset.
            break;
        };
        // Each phase cut to the budget's per-call allowance.
        retire(passive, adapter, &guard, resource_id, Limits::capped(cap_ms));
    }
}

/// The destination is destroyed: retire its guest blob and forget its record (a poisoned one
/// stays, pin and all, until the generation ends). PASSIVE, content transaction held.
pub(crate) fn destination_gone(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    guard: &SystemBackingGuard<'_>,
    resource_id: u32,
) {
    if adapter.system_backings.guest_record(resource_id).is_none() {
        return;
    }
    before_lease_change(passive, adapter, guard, resource_id);
    guard.guest_forget(resource_id);
}
