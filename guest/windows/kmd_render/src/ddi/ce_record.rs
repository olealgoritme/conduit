//! The copy-engine Present record (`'HEF3'`): the I/O half of M3c-0. `DxgkDdiRender` calls
//! [`note_render`] for a `HERF` / `HEPR` whose RM fence tail took the fence path; this file reads
//! the record behind the tail (`HERF` offset 72, `HEPR` offset 96), validates it with the
//! protocol's parser (`HeliosRmFenceTailV3::validate`, `matches_fence`), counts what it saw and
//! stashes a valid record on the context beside the fence's boundary. Nothing acts on the record:
//! the fence keeps its own path (`attach_rm_fence_marker`), the Present DDI is untouched. The
//! pure rules are `helios_kmd_logic::ce_record`; the design is `docs/rm-copy-engine-present.md`
//! sections 10, 11.1 and 13.
//!
//! Counters (`helios_kmd_logic::ce_record::COUNTERS`, written only here): `CeRecSeen` records
//! kept, `CeRecBad` refused with `CeRecWhy` the last reason and `CeRecMask` every reason seen,
//! `CeRecNoCopy` FENCE tails without a record, `CeRecLast` / `CeRecMod` the latest kept record's
//! `semaphore.h_client` and `source.modifier`. Atomics in the Render DDI; the registry from the
//! Render DDI (PASSIVE) only on the first event, a new refusal reason and every 64th refusal /
//! 256th record, the rest through [`publish_counters`] with the NVRM counter block.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use helios_kmd_logic::ce_record::{self as cr, ClientCheck, Outcome, Parsed, Why};
use helios_protocol::{
    HeliosRmFenceTail, HeliosRmFenceTailV3, TailV3, TailV3Error, HELIOS_RM_FENCE_TAIL_V3_BYTES,
};

use crate::device::{ContextHandleRef, StashedCeRecord};

static SEEN: AtomicU32 = AtomicU32::new(0);
static BAD: AtomicU32 = AtomicU32::new(0);
static LAST_WHY: AtomicU32 = AtomicU32::new(0);
static WHY_MASK: AtomicU32 = AtomicU32::new(0);
static NO_COPY: AtomicU32 = AtomicU32::new(0);
static LAST_CLIENT: AtomicU32 = AtomicU32::new(0);
static LAST_MODIFIER: AtomicU64 = AtomicU64::new(0);
/// The `CeRecMod` value last written: a REG_QWORD write is outside the registry mirror's
/// changed-only cache, so [`publish_counters`] writes it only when it changed.
static MODIFIER_WRITTEN: AtomicU64 = AtomicU64::new(0);

/// The protocol's reason as the counter's code (`kmd_logic` cannot name the protocol's enum; a
/// test there checks every arm below by name).
fn why_of(e: TailV3Error) -> Why {
    match e {
        TailV3Error::BadMagic => Why::BadMagic,
        TailV3Error::Short => Why::Short,
        TailV3Error::Version => Why::Version,
        TailV3Error::Flags => Why::Flags,
        TailV3Error::Incomplete => Why::Incomplete,
        TailV3Error::Reserved => Why::Reserved,
        TailV3Error::Handle => Why::Handle,
        TailV3Error::SemaphoreOffset => Why::SemaphoreOffset,
        TailV3Error::Value => Why::Value,
        TailV3Error::Dimensions => Why::Dimensions,
        TailV3Error::Format => Why::Format,
        TailV3Error::Pitch => Why::Pitch,
        TailV3Error::Modifier => Why::Modifier,
        TailV3Error::Size => Why::Size,
    }
}

/// The record at `offset` of the command, through the protocol's parser. The first 96 bytes are
/// copied once into a local (the command's bytes are read exactly once); the command's own length
/// is the bound for the record's `bytes` (a later revision may be longer).
///
/// # Safety
/// `cmd_len` bytes are readable at `command`.
unsafe fn read_record(command: *const u8, cmd_len: usize, offset: usize) -> TailV3 {
    let Some(available) = cr::available(cmd_len, offset) else {
        return TailV3::Absent;
    };
    let take = available.min(HELIOS_RM_FENCE_TAIL_V3_BYTES);
    let mut local = [0u8; HELIOS_RM_FENCE_TAIL_V3_BYTES];
    // SAFETY: `offset + take <= cmd_len` readable bytes; the local holds `take`.
    unsafe { core::ptr::copy_nonoverlapping(command.add(offset), local.as_mut_ptr(), take) };
    if take < HELIOS_RM_FENCE_TAIL_V3_BYTES {
        // Absent (fewer than 4 bytes or a zero magic), a foreign magic, or Short.
        return HeliosRmFenceTailV3::parse(&local[..take]);
    }
    if local[..4] == [0u8; 4] {
        return TailV3::Absent;
    }
    // SAFETY: `local` holds exactly one record's bytes; the type is plain old data.
    let record =
        unsafe { core::ptr::read_unaligned(local.as_ptr().cast::<HeliosRmFenceTailV3>()) };
    match record.validate(available) {
        Ok(()) => TailV3::Record(record),
        Err(e) => TailV3::Reject(e),
    }
}

/// The `h_client` rule (M3c-2): a record's `semaphore.h_client` and `source.h_client` must be RM
/// clients the PRESENTING process created itself. What is compared: `process`, the
/// `hKmdProcess` token of the D3DKMT device that owns the presenting context
/// (`ContextHandleRef::creator_process`), against the token the KMD recorded with `h_client`
/// when it forwarded the `NV_ESC_RM_ALLOC` of that client's root object: the escape's own device
/// (`DeviceHandleRef::creator_process` of `hDevice`, `ddi/escape.rs`), stored in the client table
/// beside the owner (`nvrm_clients::ClientTable::process_of`). One process has one
/// `hKmdProcess` for all its devices (the token the present-stream registration already uses
/// to tie the ICD's device to the runtime's), and a client leaves the table with its free, its
/// file's close, its device's destruction and the transport, so a token of a dead process names
/// no client. `Unknown` (refused by the route) whenever the table cannot say: hardening off
/// (`NvDupHarden` 0 records nothing), a full table, no presenter, no transport. Spinlock only.
pub(crate) fn record_client_owned_by_presenter(
    adapter: &crate::adapter::AdapterContext,
    process: usize,
    h_client: u32,
) -> ClientCheck {
    let recorded = adapter
        .with_virtio(|v| v.nvrm_client_process(h_client))
        .ok()
        .flatten();
    cr::client_check(process, recorded)
}

/// A `HERF` / `HEPR` Render whose RM fence `tail` went to `attach_or_take_fence_tail`: read the
/// record at `offset` (`HELIOS_RM_FENCE_TAIL_V3_HERF_OFFSET` / `_HEPR_OFFSET`), count it, and
/// stash a valid one beside `boundary` (the attached fence's; `None` when the fence was not
/// attached, then nothing is stashed). Never fails the Render and changes nothing about the
/// fence. PASSIVE (`DxgkDdiRender`).
///
/// # Safety
/// `cmd_len` bytes are readable at `command` (the Render's `CommandLength`).
pub(crate) unsafe fn note_render(
    context: &ContextHandleRef<'_>,
    command: *const u8,
    cmd_len: usize,
    offset: usize,
    tail: &HeliosRmFenceTail,
    boundary: Option<u64>,
) {
    if !tail.is_fence() {
        // A record is read only behind a FENCE tail: nothing to parse, count or stash.
        return;
    }
    // SAFETY: the caller's contract.
    let parsed = unsafe { read_record(command, cmd_len, offset) };
    let (pure, record) = match parsed {
        TailV3::Absent => (Parsed::Absent, None),
        TailV3::Reject(e) => (Parsed::Reject(why_of(e)), None),
        TailV3::Record(r) => (Parsed::Record, Some(r)),
    };
    let matches = record.is_some_and(|r| r.matches_fence(tail));
    let stash = match cr::classify(true, pure, matches) {
        Outcome::NotFence => None,
        Outcome::NoCopy => {
            let n = NO_COPY.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            if cr::publish_now(n) {
                crate::diag::record_named_bytes(b"CeRecNoCopy", n);
            }
            None
        }
        Outcome::Bad(why) => {
            note_bad(why);
            None
        }
        Outcome::Seen => record.map(|record| {
            let n = SEEN.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            LAST_CLIENT.store(record.semaphore.h_client, Ordering::Relaxed);
            LAST_MODIFIER.store(record.source.modifier, Ordering::Relaxed);
            if cr::publish_now(n) {
                crate::diag::record_named_bytes(b"CeRecSeen", n);
                crate::diag::record_named_bytes(b"CeRecLast", record.semaphore.h_client);
                crate::diag::record_named_qword(b"CeRecMod", record.source.modifier);
                MODIFIER_WRITTEN.store(record.source.modifier, Ordering::Relaxed);
            }
            // The `h_client` rule is the route's (`record_client_owned_by_presenter`, at the
            // Present that would use the record, `ddi/ce_present_route.rs`): the stash keeps
            // what the producer sent, and nothing acts on it before that check.
            record
        }),
    };
    context.stash_ce_record(match (stash, boundary) {
        (Some(record), Some(boundary)) if boundary != 0 => {
            Some(StashedCeRecord { boundary, record })
        }
        _ => None,
    });
}

fn note_bad(why: Why) {
    let n = BAD.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    LAST_WHY.store(why.code(), Ordering::Relaxed);
    let seen = WHY_MASK.fetch_or(why.bit(), Ordering::Relaxed);
    if cr::publish_bad_now(n, seen, why) {
        crate::diag::record_named_bytes(b"CeRecBad", n);
        crate::diag::record_named_bytes(b"CeRecWhy", why.code());
        crate::diag::record_named_bytes(b"CeRecMask", seen | why.bit());
    }
}

/// A new generation (StartDevice): zero the counters and write zeros over their service-key
/// values, so a block an earlier run left is never read as this one's. PASSIVE.
pub(crate) fn reset_for_start() {
    for c in [&SEEN, &BAD, &LAST_WHY, &WHY_MASK, &NO_COPY, &LAST_CLIENT] {
        c.store(0, Ordering::Relaxed);
    }
    LAST_MODIFIER.store(0, Ordering::Relaxed);
    MODIFIER_WRITTEN.store(0, Ordering::Relaxed);
    use crate::diag::record_named_bytes as rec;
    rec(b"CeRecSeen", 0);
    rec(b"CeRecBad", 0);
    rec(b"CeRecWhy", 0);
    rec(b"CeRecMask", 0);
    rec(b"CeRecNoCopy", 0);
    rec(b"CeRecLast", 0);
    crate::diag::record_named_qword(b"CeRecMod", 0);
}

/// Mirror the counters to the service key once a fenced marker was seen. PASSIVE only; with the
/// NVRM counters (`publish_nvrm_counters`).
pub(crate) fn publish_counters() {
    let seen = SEEN.load(Ordering::Relaxed);
    let bad = BAD.load(Ordering::Relaxed);
    let no_copy = NO_COPY.load(Ordering::Relaxed);
    if seen == 0 && bad == 0 && no_copy == 0 {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"CeRecSeen", seen);
    rec(b"CeRecBad", bad);
    rec(b"CeRecWhy", LAST_WHY.load(Ordering::Relaxed));
    rec(b"CeRecMask", WHY_MASK.load(Ordering::Relaxed));
    rec(b"CeRecNoCopy", no_copy);
    rec(b"CeRecLast", LAST_CLIENT.load(Ordering::Relaxed));
    let modifier = LAST_MODIFIER.load(Ordering::Relaxed);
    if MODIFIER_WRITTEN.swap(modifier, Ordering::Relaxed) != modifier {
        crate::diag::record_named_qword(b"CeRecMod", modifier);
    }
}
