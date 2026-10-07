//! The copy-engine shadow mode (`RmCopyEngine` = 3, milestone M3c-1): the I/O half. The rules
//! (sampling, skips and strikes, coverage, the comparison and its bins, the counter names) are
//! `helios_kmd_logic::ce_shadow`; the hardware procedure and what each outcome means are
//! `docs/rm-copy-engine-present.md` 14.
//!
//! WHAT IT DOES. The Present's Blt arm calls [`note_present`] once its production copy is
//! settled into the destination (the legacy arm: after the copy, its wait and the mirror) or
//! handed off (the `BltAsync` arms: the copy is submitted or queued, the destination owned by it).
//! One Present in `CeShadowEvery` (default 64) becomes the SAMPLE: its `'HEF3'` record (taken
//! from its context's stash for its own boundary) and its destination. A later Present to the same
//! destination before the worker took the sample replaces it (a newer frame), so a destination
//! that is settled always holds the sample's frame or a newer one. The HPD worker
//! ([`service`]), at PASSIVE, once the destination is SETTLED (`present_buffer_settled`: no KMD
//! writer, no mirror, no queued copy; the production copy is done), takes the sample and:
//!
//! 1. pins the destination's system pages (`SystemBackingSnapshot::reader`), under a TRY of the
//!    content transaction, and checks they cover the surface and are not marked stale;
//! 2. brings the channel up if needed (`ce_channel::ensure_up`), dups and maps the record's
//!    semaphore and image (`ce_dup::dup_map_record`), and keeps a scratch buffer of the
//!    destination's size in the channel's client (RM system memory, a CPU view, a GPU mapping);
//! 3. fills the scratch with [`cs::POISON`], submits `acquire(record value) + copy(source ->
//!    scratch, the destination's pitch, `remap_for(record fourcc, destination format)`) + release`
//!    on the channel, and polls the completion for at most `COPY_DEADLINE_MS`;
//! 4. compares the scratch with the destination's pages row by row, and publishes the verdict.
//!
//! SAFETY OF THE PRESENT PATH. With `RmCopyEngine` 0, 1 or 2 [`note_present`] is one relaxed load
//! and nothing else here runs (the worker's entry is not reached). In shadow mode the Present path
//! pays a leaf spinlock and, for a sampled Present, the context's stash spinlock and a
//! `KeSetEvent`; it never waits on anything the shadow does. The worker never waits for a lock
//! the Present path takes: the content transaction is TRIED (a busy one is retried next pass), the
//! virtio spinlock is held only for the ownership check. Reading the pages after the transaction
//! ended is memory-safe (the leases stay locked by the reader); a Present that writes them during
//! the compare is detected (`CeShadowRace`), never prevented.
//!
//! LOCKS, in order (`docs/rm-copy-engine-present.md` 14.3): Present: `SAMPLE` (leaf), released,
//! then the context's `ce_record` (leaf). Worker: the channel's `IO_BUSY` (a flag, try only) ->
//! the content transaction (try only) -> the virtio spinlock (inside it, as the order content ->
//! Venus -> virtio allows), both released before any RM message; then the channel's `STATE`,
//! `ce_dup`'s `CACHE` and `SCRATCH` (leaf spinlocks, plain data, never held across I/O); the
//! virtio spinlock once more for the race check. No Venus mutex, no scanout lock.
//!
//! FAILURES. Every one is counted (`CeShadowSkip`, `CeShadowWhy`, `CeShadowMask`) and none is
//! fatal; the strikes of `Skip::strikes` (three) disable the mode for the transport generation,
//! and the channel is torn down then (the dups and the scratch with it). A copy that did not
//! complete is a stuck channel: the producer's value is read through the KMD's mapping
//! (`CeShadowSem`) and the channel is torn down (the group's free stops it; the dups and the
//! scratch go after it).

use core::sync::atomic::{AtomicU32, Ordering};

use super::ce_channel::{self as ce, CpuView, GpuMap, Handles, NotUp};
use super::ce_dup;
use super::Io;
use crate::adapter::AdapterContext;
use crate::device::{ContextHandleRef, StashedCeRecord};
use crate::irql::PassiveLevel;
use crate::sync::SpinLock;
use alloc::vec::Vec;
use helios_kmd_logic::ce_present as cp;
use helios_kmd_logic::ce_shadow::{self as cs, Bin, Capture, Skip, Tally};
use helios_kmd_logic::rm_ce_channel as cc;
use helios_kmd_logic::rm_client::{self as rc, Fail, FailKind};
use helios_kmd_logic::sweep_budget::UNITS_PER_MS;

/// The destination of a sampled Present, as the Blt arm resolved it.
#[derive(Clone, Copy)]
pub(crate) struct ShadowDst {
    pub resource_id: u32,
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    /// The DXGI format of the destination (`ce_present::dst_fourcc_for_dxgi`).
    pub dxgi_format: u32,
}

#[derive(Clone, Copy)]
struct Sample {
    record: StashedCeRecord,
    dst: ShadowDst,
    /// When the sample (or the frame that re-targeted it) was taken, 100 ns.
    since: u64,
}

/// The scratch destination in the channel's client.
#[derive(Clone, Copy)]
struct Scratch {
    cpu: CpuView,
    gpu: GpuMap,
    bytes: u64,
}

static SAMPLE: SpinLock<Option<Sample>> = SpinLock::new(None);
static SCRATCH: SpinLock<Option<Scratch>> = SpinLock::new(None);
/// The destination of the sample being copied or compared (0: none).
static IN_FLIGHT: AtomicU32 = AtomicU32::new(0);
/// Presents to the sample's destination (pending or in flight) seen by [`note_present`]: a change
/// during a compare is a race.
static DST_GEN: AtomicU32 = AtomicU32::new(0);
static EVERY: AtomicU32 = AtomicU32::new(cs::EVERY_DEFAULT);
static STRIKES: AtomicU32 = AtomicU32::new(0);
static DISABLED: AtomicU32 = AtomicU32::new(0);

static SEEN: AtomicU32 = AtomicU32::new(0);
static N: AtomicU32 = AtomicU32::new(0);
static OK: AtomicU32 = AtomicU32::new(0);
static BAD: AtomicU32 = AtomicU32::new(0);
static PCT: AtomicU32 = AtomicU32::new(0);
static ROW: AtomicU32 = AtomicU32::new(0);
static SKIP: AtomicU32 = AtomicU32::new(0);
static WHY: AtomicU32 = AtomicU32::new(0);
static MASK: AtomicU32 = AtomicU32::new(0);
static P100: AtomicU32 = AtomicU32::new(0);
static P99: AtomicU32 = AtomicU32::new(0);
static P90: AtomicU32 = AtomicU32::new(0);
static PLOW: AtomicU32 = AtomicU32::new(0);
static SWP: AtomicU32 = AtomicU32::new(0);
static SWPCT: AtomicU32 = AtomicU32::new(0);
static RACE: AtomicU32 = AtomicU32::new(0);
static COPY_US: AtomicU32 = AtomicU32::new(0);
static DUP_US: AtomicU32 = AtomicU32::new(0);
static CMP_US: AtomicU32 = AtomicU32::new(0);
static SEM: AtomicU32 = AtomicU32::new(0);

/// The deadline of one attempt's RM messages (the dups, the maps, the scratch: about ten).
const ATTEMPT_BUDGET_MS: u64 = 2_000;

// ---- the Present path ------------------------------------------------------------------------------

/// The Blt arm of `DxgkDdiPresent` (PASSIVE), after its production copy into `dst` is settled or
/// handed off. One relaxed load unless `RmCopyEngine` is 3. Never fails, never waits.
#[inline(never)]
pub(crate) fn note_present(
    adapter: &AdapterContext,
    context: Option<&ContextHandleRef<'_>>,
    boundary: Option<u64>,
    dst: ShadowDst,
) {
    if !ce::shadow_mode() {
        return;
    }
    let seen = SEEN.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let in_flight = IN_FLIGHT.load(Ordering::Acquire);
    if in_flight != 0 && in_flight == dst.resource_id {
        DST_GEN.fetch_add(1, Ordering::AcqRel);
    }
    if DISABLED.load(Ordering::Relaxed) != 0 {
        return;
    }
    let pending = SAMPLE.lock().map(|s| s.dst.resource_id);
    let what = cs::capture(
        u64::from(seen),
        EVERY.load(Ordering::Relaxed),
        pending,
        in_flight != 0,
        dst.resource_id,
    );
    if what == Capture::Ignore {
        return;
    }
    // The context's stash of this Present's own boundary (taken: nothing else reads it).
    let record = match (context, boundary) {
        (Some(c), Some(b)) => c.take_ce_record(b),
        _ => None,
    };
    let mut slot = SAMPLE.lock();
    // Still what was decided (the worker may have taken the pending sample meanwhile).
    let still = match what {
        Capture::Take => slot.is_none() && IN_FLIGHT.load(Ordering::Acquire) == 0,
        Capture::Retarget => slot.is_some_and(|s| s.dst.resource_id == dst.resource_id),
        Capture::Ignore => false,
    };
    if !still {
        return;
    }
    let Some(record) = record else {
        // A newer frame without a record: the pending one's frame is overwritten, drop it.
        *slot = None;
        drop(slot);
        skip(Skip::NoRecord);
        return;
    };
    *slot = Some(Sample {
        record,
        dst,
        since: ce::now_100ns(),
    });
    drop(slot);
    adapter.signal_hpd();
}

/// Count a skip (atomics only: callable from the Present path).
fn skip(why: Skip) {
    SKIP.fetch_add(1, Ordering::Relaxed);
    WHY.store(why.code(), Ordering::Relaxed);
    MASK.fetch_or(why.bit(), Ordering::Relaxed);
}

// ---- the worker ----------------------------------------------------------------------------------

/// One HPD worker pass in shadow mode (`ce_channel::service`, PASSIVE). Nothing to do without a
/// sample; a sample whose destination is not settled yet (or whose content transaction is busy)
/// waits for a later pass, at most `EXPIRE_MS`.
#[inline(never)]
pub(super) fn service(passive: PassiveLevel, adapter: &AdapterContext) {
    let Some(sample) = *SAMPLE.lock() else {
        return;
    };
    if DISABLED.load(Ordering::Relaxed) != 0 {
        drop_sample(Skip::Disabled);
        return;
    }
    let now = ce::now_100ns();
    if now.saturating_sub(sample.since) > cs::EXPIRE_MS * UNITS_PER_MS {
        drop_sample(Skip::Expired);
        return;
    }
    if !adapter.display_half() || adapter.hpd_stop.load(Ordering::Acquire) != 0 {
        return;
    }
    let Ok(epoch) = adapter.with_virtio(|v| v.nvrm_epoch()) else {
        return;
    };
    if epoch == 0 {
        return;
    }
    if !ce::try_io() {
        return;
    }
    attempt(passive, adapter, epoch, sample.dst.resource_id);
    ce::end_io();
    ce::publish_counters();
}

fn drop_sample(why: Skip) {
    if SAMPLE.lock().take().is_some() {
        skip(why);
        publish_counters();
    }
}

/// The destination's pages, pinned, once it is settled. `Err(None)`: not yet (retry);
/// `Err(Some(skip))`: the sample is dropped.
fn pin_destination(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    id: u32,
) -> Result<crate::adapter::SystemBackingReader, Option<Skip>> {
    // TRY only: a mirror or a paging operation holding it is never kept waiting.
    let Some(guard) = adapter.system_backings.try_serialize(passive) else {
        return Err(None);
    };
    let (live, settled) = adapter
        .with_virtio(|v| (v.resource_is_live(id), v.present_buffer_settled(id)))
        .unwrap_or((false, false));
    if !live {
        return Err(Some(Skip::NoDestination));
    }
    if !settled {
        return Err(None);
    }
    if guard.system_copy_invalid(id) {
        return Err(Some(Skip::NoDestination));
    }
    let Some(snapshot) = guard.snapshot(id) else {
        return Err(Some(Skip::NoDestination));
    };
    snapshot.reader().ok_or(Some(Skip::NoDestination))
}

/// The caller holds `IO_BUSY`.
fn attempt(passive: PassiveLevel, adapter: &AdapterContext, epoch: u64, id: u32) {
    let reader = match pin_destination(passive, adapter, id) {
        Ok(r) => r,
        Err(None) => return,
        Err(Some(why)) => {
            drop_sample(why);
            return;
        }
    };
    let Some(sample) = SAMPLE.lock().take() else {
        return;
    };
    IN_FLIGHT.store(id, Ordering::Release);
    let gen0 = DST_GEN.load(Ordering::Acquire);
    N.fetch_add(1, Ordering::Relaxed);
    let result = shadow_copy(passive, adapter, epoch, &sample, &reader);
    // The reader (the pin) goes now, at PASSIVE, before the verdict is published.
    drop(reader);
    match result {
        Ok(tally) => {
            let settled = adapter
                .with_virtio(|v| v.present_buffer_settled(id))
                .unwrap_or(false);
            let raced = DST_GEN.load(Ordering::Acquire) != gen0 || !settled;
            publish_verdict(tally, raced);
        }
        Err(why) => {
            skip(why);
            if why.strikes() {
                let strikes = STRIKES.fetch_add(1, Ordering::Relaxed) + 1;
                if strikes >= cs::MAX_STRIKES {
                    DISABLED.store(1, Ordering::Relaxed);
                }
            }
            // A stuck or broken channel, or the mode struck out: the channel goes (the scratch
            // and the dups with it, in `ce_channel::undo_all`'s order).
            if matches!(why, Skip::NotReached | Skip::Channel) || DISABLED.load(Ordering::Relaxed) != 0 {
                ce::teardown(passive, adapter, ce::budget_ms(cc::UNDO_BUDGET_MS));
            }
        }
    }
    IN_FLIGHT.store(0, Ordering::Release);
}

fn publish_verdict(t: Tally, raced: bool) {
    let v = t.verdict();
    if v.all_equal {
        OK.fetch_add(1, Ordering::Relaxed);
    } else {
        BAD.fetch_add(1, Ordering::Relaxed);
    }
    PCT.store(v.equal_pct, Ordering::Relaxed);
    ROW.store(v.row_word, Ordering::Relaxed);
    SWPCT.store(v.swapped_pct, Ordering::Relaxed);
    if v.swap_pattern {
        SWP.fetch_add(1, Ordering::Relaxed);
    }
    if raced {
        RACE.fetch_add(1, Ordering::Relaxed);
    }
    let bin = match v.bin {
        Bin::All => &P100,
        Bin::P99 => &P99,
        Bin::P90 => &P90,
        Bin::Low => &PLOW,
    };
    bin.fetch_add(1, Ordering::Relaxed);
}

fn us_since(t0: u64) -> u32 {
    (ce::now_100ns().saturating_sub(t0) / 10).min(u64::from(u32::MAX)) as u32
}

/// Steps 2 to 4 of the module docs. The caller holds `IO_BUSY` and the destination's pin.
#[inline(never)]
fn shadow_copy(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    epoch: u64,
    s: &Sample,
    reader: &crate::adapter::SystemBackingReader,
) -> Result<Tally, Skip> {
    let d = s.dst;
    let r = s.record.record;
    // The destination: its leases must cover every byte the compare reads.
    let mut runs: Vec<(u64, u64)> = Vec::new();
    let mut vas: Vec<*const u8> = Vec::new();
    if !reader.runs(&mut runs, &mut vas) {
        return Err(Skip::NoDestination);
    }
    let line = u64::from(d.width) * 4;
    if d.width == 0 || d.height == 0 || u64::from(d.pitch) < line || d.pitch % 4 != 0 {
        return Err(Skip::Unsupported);
    }
    let need = u64::from(d.height - 1) * u64::from(d.pitch) + line;
    if !cs::covers(&runs, need) {
        return Err(Skip::NoDestination);
    }
    // The pair: same extent, a format pair the remap converts.
    if r.source.width != d.width || r.source.height != d.height {
        return Err(Skip::Unsupported);
    }
    let dst_fourcc = cp::dst_fourcc_for_dxgi(d.dxgi_format).ok_or(Skip::Unsupported)?;
    let remap = cp::remap_for(r.source.fourcc, dst_fourcc).map_err(|_| Skip::Unsupported)?;
    // The channel (its first bring-up runs here, on its own deadline).
    match ce::ensure_up(passive, adapter, epoch) {
        Ok(()) => {}
        Err(NotUp::Refused(_)) | Err(NotUp::Failed(..)) => return Err(Skip::Channel),
    }
    let (Some(h), Some(gen)) = (ce::handles(), ce::gen()) else {
        return Err(Skip::Channel);
    };
    let plan = cp::source_plan(gen, &ce_dup::source_desc(&s.record)).map_err(|_| Skip::Unsupported)?;
    let bytes = helios_kmd_logic::round_up_page(u64::from(d.pitch) * u64::from(d.height));
    let (producer, scratch) = {
        // Every wait primitive below obeys the attempt's deadline; the section ends before the
        // copy's poll (its own deadline) and the compare.
        let _bounded = crate::ddi::escape_wait::begin_bounded(ATTEMPT_BUDGET_MS as u32);
        let io = Io {
            passive,
            adapter,
            epoch,
            limit: Some(ce::budget_ms(ATTEMPT_BUDGET_MS)),
        };
        let t0 = ce::now_100ns();
        let refused_before = ce_dup::dup_failures();
        let producer = match ce_dup::dup_map_record(&io, &h, &s.record, gen) {
            Ok(p) => p,
            Err(f) if f.kind == FailKind::Layout => return Err(Skip::Unsupported),
            Err(_) if ce_dup::dup_failures() != refused_before => return Err(Skip::DupRefused),
            Err(_) => return Err(Skip::MapFailed),
        };
        DUP_US.store(us_since(t0), Ordering::Relaxed);
        let scratch = ensure_scratch(&io, &h, bytes).map_err(|_| Skip::Scratch)?;
        (producer, scratch)
    };
    // A copy that writes nothing never compares equal by accident.
    poison(&scratch, bytes);
    let rect = cp::CopyRect {
        src_va: producer.src_va,
        dst_va: scratch.gpu.va,
        src_pitch: r.source.pitch,
        dst_pitch: d.pitch,
        line_bytes: plan.line_bytes,
        lines: d.height,
        layout: plan.layout,
        dst_layout: cp::SurfaceLayout::Pitch,
        remap,
        stamp: None,
    };
    let acquire = cp::Acquire {
        va: producer.sem_va,
        value: r.semaphore.value,
    };
    let kicked = ce::now_100ns();
    let value = ce::submit(acquire, &rect).map_err(|_| Skip::Channel)?;
    wait_copy(passive, adapter, epoch, &h, s, value, kicked)?;
    COPY_US.store(us_since(kicked), Ordering::Relaxed);
    let t0 = ce::now_100ns();
    let tally = compare(&d, dst_fourcc, &scratch, &runs, &vas)?;
    CMP_US.store(us_since(t0), Ordering::Relaxed);
    Ok(tally)
}

/// Poll the completion up to `COPY_DEADLINE_MS` (spinning first, then in ticks). A copy not done
/// by then: the producer's value as the KMD's mapping reads it goes to `CeShadowSem`.
fn wait_copy(
    passive: PassiveLevel,
    adapter: &AdapterContext,
    epoch: u64,
    h: &Handles,
    s: &Sample,
    value: u64,
    kicked: u64,
) -> Result<(), Skip> {
    let deadline = kicked + cs::COPY_DEADLINE_MS * UNITS_PER_MS;
    let spin_until = kicked + cs::SPIN_US * 10;
    loop {
        let p = ce::poll().ok_or(Skip::Channel)?;
        if p.notifier != 0 {
            return Err(Skip::Channel);
        }
        if p.completed >= value {
            return Ok(());
        }
        let now = ce::now_100ns();
        if now >= deadline {
            let io = Io {
                passive,
                adapter,
                epoch,
                limit: Some(ce::budget_ms(cc::UNDO_BUDGET_MS)),
            };
            let seen = ce_dup::read_semaphore(&io, h, &s.record).map_or(u32::MAX, |v| v as u32);
            SEM.store(seen, Ordering::Relaxed);
            return Err(Skip::NotReached);
        }
        if now < spin_until {
            core::hint::spin_loop();
        } else {
            crate::virtio::ctrl::sleep_ms(passive, 1);
        }
    }
}

fn poison(scratch: &Scratch, bytes: u64) {
    let mut off = 0;
    while off + 4 <= bytes.min(scratch.cpu.len) {
        // SAFETY: inside the scratch's CPU view (`bytes <= len`), mapped while it is in `SCRATCH`,
        // which only this thread (holding `IO_BUSY`) changes.
        unsafe { ce::wr32(scratch.cpu.va, off, cs::POISON) };
        off += 4;
    }
    ce::full_barrier();
}

/// Row by row: the destination's row gathered from its leases, the scratch's row read through its
/// CPU view (the same pitch).
fn compare(
    d: &ShadowDst,
    dst_fourcc: u32,
    scratch: &Scratch,
    runs: &[(u64, u64)],
    vas: &[*const u8],
) -> Result<Tally, Skip> {
    let width = d.width as usize;
    let line = u64::from(d.width) * 4;
    let mut bytes: Vec<u8> = Vec::new();
    let mut dst: Vec<u32> = Vec::new();
    let mut src: Vec<u32> = Vec::new();
    if bytes.try_reserve_exact(width * 4).is_err()
        || dst.try_reserve_exact(width).is_err()
        || src.try_reserve_exact(width).is_err()
    {
        return Err(Skip::NoDestination);
    }
    bytes.resize(width * 4, 0);
    dst.resize(width, 0);
    src.resize(width, 0);
    let mask = cs::dst_mask(dst_fourcc);
    let mut tally = Tally::default();
    for y in 0..d.height {
        let start = u64::from(y) * u64::from(d.pitch);
        if start + line > scratch.cpu.len {
            return Err(Skip::Scratch);
        }
        let mut ok = true;
        let covered = cs::span_pieces(runs, start, line, |i, off, at, n| {
            let (Some(&va), Ok(off), Ok(at), Ok(n)) = (
                vas.get(i),
                usize::try_from(off),
                usize::try_from(at),
                usize::try_from(n),
            ) else {
                ok = false;
                return;
            };
            let Some(out) = bytes.get_mut(at..at + n) else {
                ok = false;
                return;
            };
            // SAFETY: `off + n` lies inside lease `i` (`span_pieces` stays inside each run, whose
            // length is the lease's), which the reader keeps locked and mapped; `out` is `n`
            // bytes of the row buffer. The pages may be written concurrently by a later Present
            // (detected as a race); the read is of plain bytes, never trusted beyond the compare.
            unsafe { core::ptr::copy_nonoverlapping(va.add(off), out.as_mut_ptr(), n) };
        });
        if !covered || !ok {
            return Err(Skip::NoDestination);
        }
        for (x, (word, chunk)) in dst.iter_mut().zip(bytes.chunks_exact(4)).enumerate() {
            *word = u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            // SAFETY: `start + 4 x + 4 <= start + line <= len` (checked above), inside the
            // scratch's CPU view; 4-aligned (the pitch is a multiple of 4, the view page aligned).
            src[x] = unsafe { ce::rd32(scratch.cpu.va, start + 4 * x as u64) };
        }
        tally.row(y, &src, &dst, mask);
    }
    Ok(tally)
}

// ---- the scratch ---------------------------------------------------------------------------------

/// The scratch of at least `need` bytes: the one kept, or a new one (RM system memory of the
/// channel's cache attribute, its CPU view, its GPU mapping at `VA_SCRATCH`). A smaller one is
/// given back first.
fn ensure_scratch(io: &Io<'_>, h: &Handles, need: u64) -> Result<Scratch, Fail> {
    if let Some(s) = *SCRATCH.lock() {
        if s.bytes >= need {
            return Ok(s);
        }
    }
    release_scratch(io, h);
    if need == 0 || need > cs::SCRATCH_MAX {
        return Err(Fail::new(FailKind::Layout, 0x74));
    }
    ce::alloc_sys(io, h, cc::H_SCRATCH, need).inspect_err(|_| ce::note_rm_error())?;
    let mut cpu = match ce::cpu_map(
        io,
        h,
        rc::H_DEVICE,
        cc::H_SCRATCH,
        ce::SYSMEM,
        need,
        ce::sysmem_view_cache(),
    ) {
        Ok(v) => v,
        Err(f) => {
            ce::note_rm_error();
            if ce::rm_free(io, h, rc::H_DEVICE, cc::H_SCRATCH).is_err() {
                ce::note_soft();
            }
            return Err(f);
        }
    };
    let gpu = match ce::gpu_map(io, h, cc::H_SCRATCH_VIRT, cc::H_SCRATCH, cc::VA_SCRATCH, need) {
        Ok(g) => g,
        Err(f) => {
            ce::note_rm_error();
            let mut ok = ce::cpu_unmap(io, h, &mut cpu, true);
            ok &= ce::rm_free(io, h, rc::H_DEVICE, cc::H_SCRATCH).is_ok();
            if !ok {
                ce::note_soft();
            }
            return Err(f);
        }
    };
    let s = Scratch {
        cpu,
        gpu,
        bytes: need,
    };
    *SCRATCH.lock() = Some(s);
    Ok(s)
}

/// Give the scratch back in reverse (its CPU view, its GPU mapping, the memory). The channel's
/// teardown calls it beside `ce_dup::release_all` (`ce_channel::undo_all`); with StopDevice's
/// flag up or the deadline spent only the kernel view is unmapped (the client's close takes the
/// rest). A no-op without a scratch.
pub(super) fn release_scratch(io: &Io<'_>, h: &Handles) {
    let Some(mut s) = SCRATCH.lock().take() else {
        return;
    };
    let send = !io.stopping() && !io.limit_spent();
    let mut ok = ce::cpu_unmap(io, h, &mut s.cpu, send);
    if send {
        ok &= ce::gpu_unmap(io, h, &s.gpu);
        ok &= ce::rm_free(io, h, rc::H_DEVICE, cc::H_SCRATCH).is_ok();
    }
    if !ok {
        ce::note_soft();
    }
}

/// The transport is about to be retired (`ce_channel::drop_views`): the scratch's kernel view is
/// unmapped, nothing is sent (the sweep closes the client).
pub(super) fn drop_views() {
    if let Some(s) = SCRATCH.lock().take() {
        ce::kernel_unmap(s.cpu.va, s.cpu.len);
    }
}

/// The transport is gone (`ce_channel::forget`).
pub(super) fn forget() {
    // A scratch `drop_views` could not take stays mapped: leaked, never unmapped under a worker.
    if SCRATCH.lock().take().is_some() {
        ce::note_soft();
    }
    *SAMPLE.lock() = None;
    IN_FLIGHT.store(0, Ordering::Release);
}

// ---- counters ------------------------------------------------------------------------------------

/// StartDevice with the channel enabled (`ce_channel::reset_for_start`, PASSIVE): zero the state
/// and the counters; with `RmCopyEngine` = 3 read `CeShadowEvery` and write the block's zeros.
pub(super) fn reset_for_start(knob: u32) {
    for c in [
        &SEEN, &N, &OK, &BAD, &PCT, &ROW, &SKIP, &WHY, &MASK, &P100, &P99, &P90, &PLOW, &SWP,
        &SWPCT, &RACE, &COPY_US, &DUP_US, &CMP_US, &SEM, &STRIKES, &DISABLED, &IN_FLIGHT,
        &DST_GEN,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    *SAMPLE.lock() = None;
    if cc::mode(knob) != cc::Mode::Shadow {
        return;
    }
    let every = cs::every_in_force(crate::diag::read_config_dword(
        crate::diag::knobs::CE_SHADOW_EVERY,
        0,
    ));
    EVERY.store(every, Ordering::Relaxed);
    publish(true);
}

/// Mirror the counters (PASSIVE) once shadow mode saw a Present.
pub(super) fn publish_counters() {
    publish(false);
}

fn publish(always: bool) {
    if !always && SEEN.load(Ordering::Relaxed) == 0 {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"CeShadowEach", EVERY.load(Ordering::Relaxed));
    rec(b"CeShadowSeen", SEEN.load(Ordering::Relaxed));
    rec(b"CeShadowN", N.load(Ordering::Relaxed));
    rec(b"CeShadowOk", OK.load(Ordering::Relaxed));
    rec(b"CeShadowBad", BAD.load(Ordering::Relaxed));
    rec(b"CeShadowPct", PCT.load(Ordering::Relaxed));
    rec(b"CeShadowRow", ROW.load(Ordering::Relaxed));
    rec(b"CeShadowSkip", SKIP.load(Ordering::Relaxed));
    rec(b"CeShadowWhy", WHY.load(Ordering::Relaxed));
    rec(b"CeShadowMask", MASK.load(Ordering::Relaxed));
    rec(b"CeShadowP100", P100.load(Ordering::Relaxed));
    rec(b"CeShadowP99", P99.load(Ordering::Relaxed));
    rec(b"CeShadowP90", P90.load(Ordering::Relaxed));
    rec(b"CeShadowPLow", PLOW.load(Ordering::Relaxed));
    rec(b"CeShadowSwp", SWP.load(Ordering::Relaxed));
    rec(b"CeShadowSwPct", SWPCT.load(Ordering::Relaxed));
    rec(b"CeShadowRace", RACE.load(Ordering::Relaxed));
    rec(b"CeShadowUs", COPY_US.load(Ordering::Relaxed));
    rec(b"CeShadowDupUs", DUP_US.load(Ordering::Relaxed));
    rec(b"CeShadowCmpUs", CMP_US.load(Ordering::Relaxed));
    rec(b"CeShadowSem", SEM.load(Ordering::Relaxed));
    rec(b"CeShadowStrk", STRIKES.load(Ordering::Relaxed));
}
