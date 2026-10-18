//! The GPU-ordered window copy (`NvkBltGpuCopy`, tag `HOWW`): the I/O half. The decisions, the
//! record and the parse are `helios_kmd_logic::window_copy` (host-tested); this file keeps the
//! record, answers `HELIOS_ESCAPE_BLT_TARGET` from it, grants the import of a recorded surface,
//! reads the tag out of the `HERF` Render command and counts what it decides. Wire layout:
//! `protocol/src/window_copy.rs`.
//!
//! THE FLOW. The `RedirVram` foreign arm (`vram_redirect::foreign_arm`) notes every redirected
//! Blt of an NVK image into a VRAM redirection surface ([`note_blt`]); the Blt arm notes one into
//! a CPU-visible standard buffer (the redirection surface without GDI acceleration) as unwritable,
//! so the query says why the producer has nothing to write. The producer asks for its
//! back buffer's entry ([`query`]), imports the surface by resource id (its process presented into
//! it: [`import_granted`] lifts the opener rule of `RM_RESOURCE_IMPORT` for it), copies the frame
//! into it on its own queue right after the frame, and presents with the `HOWW` tag beside the
//! frame's RM fence. `DxgkDdiRender` stashes the tag ([`note_render`]); the Present that follows on
//! that context takes it and, when [`decide`] verifies it, completes the Blt with no copy: no
//! copy-engine route, no read-ledger claim on the source, no worker pass. The Present's DMA buffer
//! keeps the fence's boundary, so it retires when the fence fires (the RM gate's `EventReady`
//! DPC, no timed poll), which is after the producer's copy: the same point the routed copy's
//! Present retired at (after its copy), minus the copy-engine leg and the worker's poll.
//!
//! Counters (`Gw` prefix, `helios_kmd_logic::window_copy::COUNTERS`): atomics on the hot paths,
//! the registry on the first event, every 256th skip and every 64th refusal, the rest through
//! [`publish_counters`].

use core::sync::atomic::{AtomicU32, Ordering};

use helios_kmd_logic::rm_vidmem::Rect;
use helios_kmd_logic::window_copy::{
    self as wc, Facts, Parsed, Surface, Table, Tag, Target, Verdict, Why,
};

use crate::ddi::create_allocation::PresentAllocInfo;
use crate::device::ContextHandleRef;
use crate::sync::SpinLock;

// The wire constants `helios_kmd_logic` spells as literals are the protocol's.
const _: () = assert!(
    wc::MAGIC == helios_protocol::HELIOS_WINDOW_WRITTEN_MAGIC
        && wc::VERSION == helios_protocol::HELIOS_WINDOW_WRITTEN_VERSION
        && wc::TAG_BYTES == core::mem::size_of::<helios_protocol::HeliosWindowWrittenTag>()
        && wc::HERF_OFFSET == helios_protocol::HELIOS_WINDOW_WRITTEN_HERF_OFFSET
        && wc::HERF_BYTES == helios_protocol::HELIOS_WINDOW_WRITTEN_HERF_BYTES
);

static TAGS: AtomicU32 = AtomicU32::new(0);
static SKIPS: AtomicU32 = AtomicU32::new(0);
static REJECTS: AtomicU32 = AtomicU32::new(0);
static LAST_WHY: AtomicU32 = AtomicU32::new(0);
static WHY_MASK: AtomicU32 = AtomicU32::new(0);
static QUERIES: AtomicU32 = AtomicU32::new(0);
static FOUND: AtomicU32 = AtomicU32::new(0);
static GRANTS: AtomicU32 = AtomicU32::new(0);
static NOTES: AtomicU32 = AtomicU32::new(0);
static LAST_TICKET: AtomicU32 = AtomicU32::new(0);
static UNWRITABLE: AtomicU32 = AtomicU32::new(0);

/// The record. A LEAF spinlock: its holders call only the pure table. Taken at PASSIVE by the
/// Present, the Render-side escape and the import's authorization (before the device lock).
static TABLE: SpinLock<Table> = SpinLock::new(Table::new());

/// The redirected Blt of an NVK image `source` into the VRAM redirection surface `destination`
/// (described by `surface`), by the process of `context`, with these rectangles. PASSIVE (the
/// Present). One table update; nothing when the context's process is unknown.
pub(crate) fn note_blt(
    context: Option<&ContextHandleRef<'_>>,
    source: &PresentAllocInfo,
    destination: &PresentAllocInfo,
    surface: Surface,
    src: Rect,
    dst: Rect,
    writable: bool,
) {
    let Some(process) = context.and_then(ContextHandleRef::creator_process) else {
        return;
    };
    let ticket = TABLE.lock().note_blt(Target {
        source: source.resource_id,
        destination: destination.resource_id,
        process: process as u64,
        src,
        dst,
        surface,
        writable,
        ticket: 0,
    });
    if !writable {
        let n = UNWRITABLE.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        // Once, then every 4096th: the reason a producer's query finds nothing to write.
        if n == 1 || n % 4096 == 0 {
            crate::diag::record_named_bytes(b"GwUnwritable", n);
        }
    }
    if ticket != 0 {
        let n = NOTES.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if LAST_TICKET.swap(ticket, Ordering::Relaxed) != ticket || n == 1 {
            crate::diag::record_named_bytes(b"GwNote", n);
            crate::diag::record_named_bytes(b"GwTicket", ticket);
        }
    }
}

/// `HELIOS_ESCAPE_BLT_TARGET`: the entry for `source` of `process`, if any. PASSIVE.
pub(crate) fn query(process: usize, source: u32) -> Option<Target> {
    QUERIES.fetch_add(1, Ordering::Relaxed);
    let found = TABLE.lock().lookup(source, process as u64);
    if found.is_some() {
        let n = FOUND.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if n == 1 {
            crate::diag::record_named_bytes(b"GwQuery", QUERIES.load(Ordering::Relaxed));
            crate::diag::record_named_bytes(b"GwQFound", n);
        }
    }
    found
}

/// Whether `process` presented into `resource_id` (the record holds such an entry): its NVK may
/// import that surface by resource id. PASSIVE, no other lock held (the caller takes the device
/// lock after this).
pub(crate) fn import_granted(process: usize, resource_id: u32) -> bool {
    let granted = TABLE.lock().granted(process as u64, resource_id);
    if granted {
        let n = GRANTS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
        if n == 1 || n % 64 == 0 {
            crate::diag::record_named_bytes(b"GwGrant", n);
        }
    }
    granted
}

/// The surface `resource_id` is being destroyed (`vidmem::released`): no record may name it.
pub(crate) fn destination_gone(resource_id: u32) {
    let _ = TABLE.lock().destination_gone(resource_id);
}

/// `DxgkDdiRender` of a valid `HERF` command: read the tag out of its tail and stash it for the
/// Present that follows on `context`. A command with no tag clears a stale stash. The ordinary
/// command costs one relaxed load.
///
/// # Safety
/// `cmd_len` bytes are readable at `command`.
pub(crate) unsafe fn note_render(
    context: &ContextHandleRef<'_>,
    command: *const u8,
    cmd_len: usize,
) {
    let parsed = if cmd_len > wc::HERF_OFFSET {
        let take = (cmd_len - wc::HERF_OFFSET).min(wc::TAG_BYTES);
        let mut tail = [0u8; wc::TAG_BYTES];
        // SAFETY: `take <= cmd_len - HERF_OFFSET`, inside the command; the local holds `take`.
        unsafe {
            core::ptr::copy_nonoverlapping(command.add(wc::HERF_OFFSET), tail.as_mut_ptr(), take);
        }
        wc::parse(&tail[..take])
    } else {
        Parsed::Absent
    };
    let stash = match parsed {
        Parsed::Absent => None,
        Parsed::Tag(tag) => {
            TAGS.fetch_add(1, Ordering::Relaxed);
            Some(tag)
        }
        Parsed::Reject(why) => {
            TAGS.fetch_add(1, Ordering::Relaxed);
            note_reject(why);
            None
        }
    };
    if context.stash_window_tag(stash) {
        note_reject(Why::Orphan);
    }
}

/// A tagged Present: may its Blt be completed with no copy? `true` is provisional: the caller
/// then takes the skip and calls [`note_skip`], or [`note_retry`] when its own preconditions
/// refuse it. `false` has been counted.
///
/// `blt`: a Blt with an allocation list, not a flip or `ColorFill`, no windowed-Blt snapshot.
/// `rects`: the Present's rectangles inside both surfaces, equal sizes (`None` otherwise).
pub(crate) fn decide(
    context: Option<&ContextHandleRef<'_>>,
    tag: Tag,
    blt: bool,
    source: Option<&PresentAllocInfo>,
    destination: Option<&PresentAllocInfo>,
    rects: Option<(Rect, Rect)>,
    boundary: bool,
) -> bool {
    let process = context
        .and_then(ContextHandleRef::creator_process)
        .unwrap_or(0) as u64;
    let surfaces = match (source, destination) {
        (Some(s), Some(d)) => s.foreign.is_some()
            && s.storage
                != crate::ddi::create_allocation::PresentAllocationStorage::PitchedStandardBuffer
            && crate::virtio::rm_client::vidmem::lookup(s.resource_id).is_none()
            && crate::virtio::rm_client::vidmem::lookup(d.resource_id).is_some(),
        _ => false,
    };
    let facts = Facts {
        blt,
        surfaces,
        source: source.map_or(0, |s| s.resource_id),
        destination: destination.map_or(0, |d| d.resource_id),
        process,
        rects,
        boundary,
    };
    let record = TABLE.lock().lookup(facts.source, process);
    match wc::verify(&tag, record.as_ref(), &facts) {
        Verdict::NoCopy => true,
        Verdict::Reject(why) => {
            note_reject(why);
            false
        }
    }
}

/// The Present was completed with no copy.
pub(crate) fn note_skip() {
    let n = SKIPS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    if n == 1 || n % 256 == 0 {
        crate::diag::record_named_bytes(b"GwTag", TAGS.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(b"GwSkip", n);
    }
}

/// The skip was verified but the Present could not take it (DMA or private buffer, patch
/// capacity): dxgkrnl retries it, and the retry carries no tag, so it is the ordinary Blt.
pub(crate) fn note_retry() {
    note_reject(Why::Retry);
}

fn note_reject(why: Why) {
    let n = REJECTS.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    LAST_WHY.store(why.code(), Ordering::Relaxed);
    let bit = 1u32 << (why.code() - 1);
    let seen = WHY_MASK.fetch_or(bit, Ordering::Relaxed);
    if n == 1 || seen & bit == 0 || n % 64 == 0 {
        crate::diag::record_named_bytes(b"GwTag", TAGS.load(Ordering::Relaxed));
        crate::diag::record_named_bytes(b"GwRej", n);
        crate::diag::record_named_bytes(b"GwRejWhy", why.code());
        crate::diag::record_named_bytes(b"GwWhyMask", seen | bit);
    }
}

/// A new generation (StartDevice): the record and the counters zeroed, zeros over their values.
pub(crate) fn reset_for_start() {
    for c in [
        &TAGS,
        &SKIPS,
        &REJECTS,
        &LAST_WHY,
        &WHY_MASK,
        &QUERIES,
        &FOUND,
        &GRANTS,
        &NOTES,
        &LAST_TICKET,
        &UNWRITABLE,
    ] {
        c.store(0, Ordering::Relaxed);
    }
    *TABLE.lock() = Table::new();
    for name in wc::COUNTERS {
        crate::diag::record_named_bytes(name.as_bytes(), 0);
    }
}

/// Mirror the counters to the service key once anything happened. PASSIVE only.
pub(crate) fn publish_counters() {
    if NOTES.load(Ordering::Relaxed) == 0
        && UNWRITABLE.load(Ordering::Relaxed) == 0
        && TAGS.load(Ordering::Relaxed) == 0
        && QUERIES.load(Ordering::Relaxed) == 0
    {
        return;
    }
    use crate::diag::record_named_bytes as rec;
    rec(b"GwTag", TAGS.load(Ordering::Relaxed));
    rec(b"GwSkip", SKIPS.load(Ordering::Relaxed));
    rec(b"GwRej", REJECTS.load(Ordering::Relaxed));
    rec(b"GwRejWhy", LAST_WHY.load(Ordering::Relaxed));
    rec(b"GwWhyMask", WHY_MASK.load(Ordering::Relaxed));
    rec(b"GwQuery", QUERIES.load(Ordering::Relaxed));
    rec(b"GwQFound", FOUND.load(Ordering::Relaxed));
    rec(b"GwGrant", GRANTS.load(Ordering::Relaxed));
    rec(b"GwNote", NOTES.load(Ordering::Relaxed));
    rec(b"GwTicket", LAST_TICKET.load(Ordering::Relaxed));
    rec(b"GwUnwritable", UNWRITABLE.load(Ordering::Relaxed));
}
