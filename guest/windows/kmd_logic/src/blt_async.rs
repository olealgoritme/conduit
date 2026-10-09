//! Asynchronous composed present (`BltAsync`) and the dropped CPU mirror (`BltNoMirror`): the
//! pure half. The I/O half is `kmd_render/src/ddi/blt_async.rs` (knobs, counters, the DDI
//! helpers) and `kmd_render/src/virtio/gpu/blt_async.rs` (the in-flight table in the transport).
//! Design, the analysis of what the DDI waited for, the hazards and the hardware checklist:
//! `docs/zero-copy-present.md`, "Asynchronous composed present (BltAsync, BltNoMirror)".
//!
//! WHAT THIS DECIDES. A Blt `DxgkDdiPresent` whose source is an adopted foreign (NVK-on-RM)
//! allocation and whose destination is a KMD standard buffer (DWM's redirection surface) used to
//! (1) submit the Venus copy on ring 1, (2) wait for its wire fence on the app thread, (3) CPU-copy
//! the whole frame into the system-memory backing VidMm may hold for the destination. The DMA
//! fence of the Present was never tied to that wait: its private record names the copy's wire
//! fence (`PresentSubmissionPrivate::gpu_fence_id`), so dxgkrnl's fence completes when the copy
//! has. The wait existed for the mirror and for the buffer ownership hand-back only.
//!
//! The decisions here are, in order:
//!
//! * [`decide`]: which route a Blt takes: the legacy synchronous one, a DIRECT asynchronous
//!   submission from the DDI (no CPU wait), or a DEFERRED one (queued in the WindowedBlt FIFO,
//!   submitted by the HPD worker once the producer's boundary has been reached and the
//!   destination can be written);
//! * [`Table`]: the in-flight state machine of the direct submissions and the ownership rule that
//!   lets several of them write one destination (they retire in ring order);
//! * [`begin`]: what acquiring the destination for a direct submission means;
//! * [`lat_bucket`]: the latency histogram both the copy and the DDI's wait are read through;
//! * [`stale_mark`]: whether a skipped mirror must leave a "system copy invalid" mark.
//!
//! Nothing here reads a clock, a lock or a handle: every rule is a function of its arguments.

use crate::scanout_read_ledger::LedgerTicket;
use crate::rm_refresh::Edge;

/// Number of latency / wait histogram buckets (`BltAsyncLat0..7`, `BltWait0..7`).
pub const BUCKETS: usize = 8;

/// Upper bounds of buckets 0..6 in microseconds; bucket 7 is everything at or above the last.
/// 250 us, 500 us, 1 ms, 2 ms, 4 ms, 8 ms, 16 ms.
pub const BUCKET_US: [u64; BUCKETS - 1] = [250, 500, 1_000, 2_000, 4_000, 8_000, 16_000];

/// The histogram bucket of a duration in 100 ns units (the unit of `KeQueryInterruptTimePrecise`).
pub const fn lat_bucket(duration_100ns: u64) -> usize {
    let us = duration_100ns / 10;
    let mut i = 0;
    while i < BUCKETS - 1 {
        if us < BUCKET_US[i] {
            return i;
        }
        i += 1;
    }
    BUCKETS - 1
}

/// A duration in 100 ns units as microseconds, saturating at `u32::MAX` (the counter width).
pub const fn us32(duration_100ns: u64) -> u32 {
    let us = duration_100ns / 10;
    if us > u32::MAX as u64 {
        u32::MAX
    } else {
        us as u32
    }
}

/// Why a Blt was not made asynchronous (`BltAsyncWhy` holds the last code; `BltAsyncMask` has bit
/// `code - 1` for every code seen). `Off` is not counted: it is the knob.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// `BltAsync` is 0.
    Off = 1,
    /// The source is not an adopted foreign resource, or a WindowedBlt snapshot accompanies the
    /// Present (which has its own deferred path).
    NotForeign = 2,
    /// The destination is not a KMD standard buffer (an image destination never waited).
    NotBuffer = 3,
    /// No producer boundary travels with the Present and the CPU mirror is still on: the mirror
    /// needs the worker, which only the deferred route has, and the deferred route needs a boundary.
    NoBoundaryMirror = 4,
    /// The boundary names a dead stream: the copy cannot be ordered after it.
    BoundaryDead = 5,
    /// The WindowedBlt FIFO (or the token) refused the request.
    QueueRefused = 6,
    /// The private data could not carry the token.
    TokenRefused = 7,
    /// The Venus copy could not be prepared or submitted without a wait.
    SubmitRefused = 8,
    /// A deferred copy for the same destination is still queued and no boundary lets this one
    /// join it: the legacy path runs after the queue drained.
    PendingNoBoundary = 9,
    /// The in-flight table had no room.
    TableFull = 10,
    /// The destination is still owned by a reader, or by a writer that is not one of the direct
    /// submissions: the legacy arm waits for it as it always did.
    DstBusy = 11,
}

impl Why {
    /// The code written to `BltAsyncWhy`.
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The bit this reason owns in `BltAsyncMask`.
    pub const fn bit(self) -> u32 {
        1 << (self as u32 - 1)
    }
}

/// What the Present says about its producer boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Boundary {
    /// No marker: the UMD waited on the CPU, or the marker was refused.
    None,
    /// A live boundary; `ready` is whether its producer has already finished.
    Live { ready: bool },
    /// A boundary of a dead stream.
    Dead,
}

/// Everything [`decide`] needs, all of it known to the DDI before any host work.
#[derive(Debug, Clone, Copy)]
pub struct Facts {
    /// `BltAsync` is on.
    pub async_on: bool,
    /// `BltNoMirror` is on.
    pub no_mirror_on: bool,
    /// The source is one the knobs act on (`class_eligible`: an adopted foreign resource the KMD
    /// copies as one, or a Venus-native one with `BltAsyncVenus`; the KMD's own record, never the
    /// creator's word).
    pub foreign_source: bool,
    /// A WindowedBlt snapshot accompanies the Present.
    pub snapshot: bool,
    /// The destination is a KMD standard buffer (the only destination that ever waited).
    pub dst_standard_buffer: bool,
    /// The producer boundary.
    pub boundary: Boundary,
    /// A deferred (WindowedBlt) request for this destination is still queued or in flight.
    pub dst_deferred_pending: bool,
    /// The direct in-flight table has room for one more.
    pub table_has_room: bool,
}

/// The route a Blt takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// The legacy synchronous arm, unchanged. `why` says why this Blt is not asynchronous.
    Legacy { why: Why },
    /// The legacy arm, but only after the deferred requests for the destination have drained:
    /// ordering the copy ahead of an older queued frame would let the older one land last.
    LegacyAfterDrain { why: Why },
    /// Submit from the DDI and return: no CPU wait. Ownership is released by the completion DPC.
    Direct,
    /// Queue in the WindowedBlt FIFO and return: the HPD worker submits it once the producer's
    /// boundary has been reached and the destination can be written.
    Deferred,
}

/// Whether `BltNoMirror` takes effect for a Blt: only for a foreign (NVK-composed) source into a
/// standard buffer. Every other Blt keeps its mirror whatever the knob says.
pub const fn no_mirror_applies(
    no_mirror_on: bool,
    foreign_source: bool,
    snapshot: bool,
    dst_standard_buffer: bool,
) -> bool {
    no_mirror_on && foreign_source && !snapshot && dst_standard_buffer
}

/// What a Blt's source is, as far as the asynchronous arm is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceClass {
    /// An adopted foreign (NVK-on-RM) allocation the KMD copies through the explicit-modifier
    /// import (`ForeignCopy=1`).
    Foreign,
    /// An adopted foreign allocation seen while `ForeignCopy` is 0: the legacy arm treats it as an
    /// ordinary OPTIMAL source (the plain import the host refuses for these resources, counted
    /// `FcOff`), so neither knob may act on it. This is what Heaven's present was on the v337.2
    /// hardware runs: every Blt counted `FcOff`, none took the asynchronous arm.
    ForeignCopyOff,
    /// A Venus-native source: an image the UMD created through Venus (cross-context or
    /// opaque-fd). The copy is ordered after the producer by the Venus ring itself.
    Venus,
}

/// Why neither knob acted on a Blt (`BltEntryWhy` holds the last code, `BltEntryMask` has bit
/// `code - 1` for every code seen). The codes are in the order [`entry`] tries them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryWhy {
    /// `BltAsync` and `BltNoMirror` are both 0 (`BltNoEntryK`).
    KnobOff = 1,
    /// A WindowedBlt snapshot accompanies the Present (`BltNoEntryM`): it has its own two-phase
    /// path, which neither knob changes.
    Snapshot = 2,
    /// The source is Venus-native and `BltAsyncVenus` is 0 (`BltNoEntryF`).
    NotForeign = 3,
    /// The source is foreign but `ForeignCopy` is 0 (`BltNoEntryFc`): the knobs cannot act on a
    /// source the KMD does not copy as a foreign one. Set `ForeignCopy=1`.
    ForeignCopyOff = 4,
    /// The destination is not a KMD standard buffer (`BltNoEntryS`).
    NotBuffer = 5,
    /// Never returned by [`entry`]: a Blt of the arm that returned before the decision (patch
    /// capacity, an unresolved allocation, format, source kind, snapshot validation, descriptors,
    /// extent). Counted as `BltNoEntryO`, derived as the arm's Blts minus the decided ones.
    Other = 6,
}

impl EntryWhy {
    /// The code written to `BltEntryWhy`.
    pub const fn code(self) -> u32 {
        self as u32
    }

    /// The bit this reason owns in `BltEntryMask`.
    pub const fn bit(self) -> u32 {
        1 << (self as u32 - 1)
    }
}

/// Everything [`entry`] needs.
#[derive(Debug, Clone, Copy)]
pub struct EntryFacts {
    /// `BltAsync` is on.
    pub async_on: bool,
    /// `BltNoMirror` is on.
    pub no_mirror_on: bool,
    /// `BltAsyncVenus` is on: both knobs also act on Venus-native sources.
    pub async_venus_on: bool,
    /// The class of the source.
    pub source: SourceClass,
    /// A WindowedBlt snapshot accompanies the Present.
    pub snapshot: bool,
    /// The destination is a KMD standard buffer.
    pub dst_standard_buffer: bool,
}

/// What [`entry`] decided (the in-flight table's record is [`Entry`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryDecision {
    /// The Blt goes to `try_async` (and its [`decide`]).
    pub async_enter: bool,
    /// `BltNoMirror` applies to the Blt, whichever arm copies it.
    pub no_mirror: bool,
    /// The first reason neither knob acts; `None` when at least one does.
    pub why: Option<EntryWhy>,
}

/// Whether the source class is one the knobs act on: a foreign source with its copy enabled, or
/// a Venus-native one when `BltAsyncVenus` says so.
pub const fn class_eligible(source: SourceClass, async_venus_on: bool) -> bool {
    match source {
        SourceClass::Foreign => true,
        SourceClass::ForeignCopyOff => false,
        SourceClass::Venus => async_venus_on,
    }
}

/// The entry decision of a Blt, made once, before [`decide`] and before the legacy arm.
///
/// The table (reasons are tried top down; `eligible` = class, destination and snapshot rows pass):
///
/// | knobs | snapshot | source | destination | result |
/// |---|---|---|---|---|
/// | both 0 | any | any | any | none: `KnobOff` |
/// | any on | yes | any | any | none: `Snapshot` |
/// | any on | no | Venus, `BltAsyncVenus` 0 | any | none: `NotForeign` |
/// | any on | no | foreign, `ForeignCopy` 0 | any | none: `ForeignCopyOff` |
/// | any on | no | eligible | not a standard buffer | none: `NotBuffer` |
/// | any on | no | eligible | standard buffer | `async_enter` = `BltAsync`, `no_mirror` = `BltNoMirror` |
///
/// What stays out of this function and in [`decide`]: the producer boundary, the in-flight table,
/// the deferred queue (they need the transport). A Blt that enters can still fall back there
/// (`BltAsyncFall`).
pub const fn entry(f: EntryFacts) -> EntryDecision {
    let why = if !f.async_on && !f.no_mirror_on {
        Some(EntryWhy::KnobOff)
    } else if f.snapshot {
        Some(EntryWhy::Snapshot)
    } else if matches!(f.source, SourceClass::ForeignCopyOff) {
        Some(EntryWhy::ForeignCopyOff)
    } else if !class_eligible(f.source, f.async_venus_on) {
        Some(EntryWhy::NotForeign)
    } else if !f.dst_standard_buffer {
        Some(EntryWhy::NotBuffer)
    } else {
        None
    };
    let eligible = why.is_none();
    EntryDecision {
        async_enter: eligible && f.async_on,
        no_mirror: eligible && f.no_mirror_on,
        why,
    }
}

/// The route of one Blt.
///
/// The table (rows are tried top down):
///
/// | condition | route |
/// |---|---|
/// | `BltAsync` 0 | legacy |
/// | not a foreign source, or a snapshot | legacy |
/// | destination is not a standard buffer | legacy |
/// | no boundary (or a dead one), a deferred request for the destination is queued | legacy after drain |
/// | no boundary, mirror on | legacy (the mirror needs the worker, the worker needs a boundary) |
/// | no boundary, mirror off | direct |
/// | dead boundary | legacy |
/// | live boundary, producer finished, mirror off, nothing queued for the destination, room | direct |
/// | live boundary otherwise | deferred |
///
/// The "nothing queued" condition is the per-destination order rule: a direct submission would
/// reach the host before an older deferred one and the older frame would land last.
pub const fn decide(f: Facts) -> Route {
    if !f.async_on {
        return Route::Legacy { why: Why::Off };
    }
    if !f.foreign_source || f.snapshot {
        return Route::Legacy {
            why: Why::NotForeign,
        };
    }
    if !f.dst_standard_buffer {
        return Route::Legacy {
            why: Why::NotBuffer,
        };
    }
    match f.boundary {
        Boundary::None => {
            // Whatever the mirror says: a copy queued for this destination is older than this
            // Present, and the legacy arm that follows must not reach the host before it.
            if f.dst_deferred_pending {
                Route::LegacyAfterDrain {
                    why: Why::PendingNoBoundary,
                }
            } else if !f.no_mirror_on {
                Route::Legacy {
                    why: Why::NoBoundaryMirror,
                }
            } else if !f.table_has_room {
                Route::Legacy {
                    why: Why::TableFull,
                }
            } else {
                Route::Direct
            }
        }
        Boundary::Dead if f.dst_deferred_pending => Route::LegacyAfterDrain {
            why: Why::BoundaryDead,
        },
        Boundary::Dead => Route::Legacy {
            why: Why::BoundaryDead,
        },
        Boundary::Live { ready } => {
            if ready && f.no_mirror_on && !f.dst_deferred_pending && f.table_has_room {
                Route::Direct
            } else {
                Route::Deferred
            }
        }
    }
}

/// Whether a skipped mirror leaves the destination's system copy marked invalid. The mark is
/// only owed when VidMm has system pages the KMD retains a lease on (`backing_exists`): without
/// them there is no stale copy a later page-in could copy over the GPU blob, and an allocation
/// that never was evicted stays out of the invalid set (which has a bounded capacity whose
/// overflow skips every page-in).
pub const fn stale_mark(no_mirror_effective: bool, backing_exists: bool) -> bool {
    no_mirror_effective && backing_exists
}

/// What acquiring the destination for a direct submission found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Own {
    /// Free: no writer and no reader still on it (`ExternalReady`, or a consumer that finished).
    Free,
    /// The KMD owns it as a writer.
    Writer,
    /// Anything else: a reader still on it, a CPU mirror, a teardown.
    Blocked,
}

/// The outcome of [`begin`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Begin {
    /// Take the ownership: the state becomes writer, and this submission is its first.
    Acquire,
    /// The destination already belongs to direct submissions in flight: join them. Ring-1
    /// submissions of one context retire in order, so the last to retire hands it back.
    Overlap,
    /// Not now.
    Busy,
}

/// Acquire the destination for a direct submission. A writer that is not one of the direct
/// in-flight submissions (a deferred copy, a CPU mirror) is NOT joined: its owner hands the
/// buffer back on its own terms.
pub const fn begin(own: Own, direct_writers: usize) -> Begin {
    match own {
        Own::Free => Begin::Acquire,
        Own::Writer if direct_writers > 0 => Begin::Overlap,
        Own::Writer | Own::Blocked => Begin::Busy,
    }
}

/// One direct submission in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    /// The wire fence of the copy (ring 1).
    pub fence_id: u64,
    /// The destination resource.
    pub resource_id: u32,
    /// Interrupt time of the submission, 100 ns units.
    pub t0: u64,
    /// The source resource the copy reads (0 = not recorded).
    pub source_id: u32,
    /// The read-ledger ticket on the source, retired when the copy retires (`NONE` = unledgered).
    pub ticket: LedgerTicket,
}

impl Entry {
    pub const fn new(fence_id: u64, resource_id: u32, t0: u64) -> Self {
        Self {
            fence_id,
            resource_id,
            t0,
            source_id: 0,
            ticket: LedgerTicket::NONE,
        }
    }

    pub const fn reading(mut self, source_id: u32, ticket: LedgerTicket) -> Self {
        self.source_id = source_id;
        self.ticket = ticket;
        self
    }
}

/// What a retired entry hands back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Done {
    pub resource_id: u32,
    pub t0: u64,
    pub source_id: u32,
    pub ticket: LedgerTicket,
    /// No other submission of the table still writes this destination: ownership goes back now.
    pub last_for_resource: bool,
}

/// The in-flight table of the direct submissions: fixed capacity, no allocation. Entries are
/// kept in submission order, which is the retirement order of ring 1.
#[derive(Debug, Clone, Copy)]
pub struct Table<const N: usize> {
    entries: [Entry; N],
    len: usize,
    peak: usize,
}

const EMPTY: Entry = Entry::new(0, 0, 0);

impl<const N: usize> Default for Table<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Table<N> {
    pub const fn new() -> Self {
        Self {
            entries: [EMPTY; N],
            len: 0,
            peak: 0,
        }
    }

    pub const fn len(&self) -> usize {
        self.len
    }

    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The most entries the table ever held at once.
    pub const fn peak(&self) -> usize {
        self.peak
    }

    pub const fn has_room(&self) -> bool {
        self.len < N
    }

    /// Entries writing `resource_id` now.
    pub fn writers(&self, resource_id: u32) -> usize {
        self.entries[..self.len]
            .iter()
            .filter(|e| e.resource_id == resource_id)
            .count()
    }

    /// Entries reading `source_id` now (0 matches nothing).
    pub fn readers(&self, source_id: u32) -> usize {
        if source_id == 0 {
            return 0;
        }
        self.entries[..self.len]
            .iter()
            .filter(|e| e.source_id == source_id)
            .count()
    }

    /// Remove and return the oldest entry (a transport generation ending: the caller retires
    /// what each holds).
    pub fn pop_oldest(&mut self) -> Option<Entry> {
        if self.len == 0 {
            return None;
        }
        let entry = self.entries[0];
        let mut i = 0;
        while i + 1 < self.len {
            self.entries[i] = self.entries[i + 1];
            i += 1;
        }
        self.len -= 1;
        self.entries[self.len] = EMPTY;
        Some(entry)
    }

    /// Record one submission. `false` (and nothing recorded) when the table is full or the fence
    /// id is 0 or not above every fence already in the table (a ring-1 fence id is monotonic; an
    /// out-of-order add would break the "last to retire hands it back" argument).
    pub fn add(&mut self, entry: Entry) -> bool {
        if self.len >= N || entry.fence_id == 0 || entry.resource_id == 0 {
            return false;
        }
        if self.len > 0 && entry.fence_id <= self.entries[self.len - 1].fence_id {
            return false;
        }
        self.entries[self.len] = entry;
        self.len += 1;
        if self.len > self.peak {
            self.peak = self.len;
        }
        true
    }

    /// Retire the entry of `fence_id` (any order: the transport may deliver completions of
    /// different rings out of order, and the table does not assume otherwise). `None` for a fence
    /// the table does not hold.
    pub fn complete(&mut self, fence_id: u64) -> Option<Done> {
        let at = self.entries[..self.len]
            .iter()
            .position(|e| e.fence_id == fence_id)?;
        let entry = self.entries[at];
        let mut i = at;
        while i + 1 < self.len {
            self.entries[i] = self.entries[i + 1];
            i += 1;
        }
        self.len -= 1;
        self.entries[self.len] = EMPTY;
        Some(Done {
            resource_id: entry.resource_id,
            t0: entry.t0,
            source_id: entry.source_id,
            ticket: entry.ticket,
            last_for_resource: self.writers(entry.resource_id) == 0,
        })
    }

    /// Forget everything (a new transport generation). The peak survives: it is a statistic.
    pub fn clear(&mut self) {
        self.entries = [EMPTY; N];
        self.len = 0;
    }
}

/// How an asynchronous copy ended its life, for the Level 5 frame edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finish {
    /// A DIRECT copy retired (the completion DPC).
    Direct,
    /// A DEFERRED copy with `BltNoMirror` retired: its ring completion is its terminal.
    DeferredNoMirror,
    /// A DEFERRED copy with the mirror on retired: the worker's mirror stage ends it, and raises
    /// the edge itself.
    DeferredMirrored,
}

/// The Level 5 (`KmdRmClient` 5) frame edge a finished asynchronous copy owes, if any. The
/// legacy arm raises `Edge::PresentBlt` after a copy that completed (a failed wait or a failed
/// mirror returns before it) and the worker's mirror stage raises `Edge::WindowedBlt`; an
/// asynchronous copy that never reaches either must raise the same edge at its own end, or a
/// destination that is the shown RM primary is never flipped. Whether the destination IS the
/// shown primary is `rm_refresh::judge`'s question, asked by the caller with this edge.
pub const fn edge_owed(finish: Finish, copy_ok: bool) -> Option<Edge> {
    if !copy_ok {
        return None;
    }
    match finish {
        Finish::Direct => Some(Edge::PresentBlt),
        Finish::DeferredNoMirror => Some(Edge::WindowedBlt),
        Finish::DeferredMirrored => None,
    }
}

/// Most ready-queue entries the worker looks at (`BltLookahead`).
pub const LOOKAHEAD_MAX: usize = 8;
/// `BltLookahead` default: 1 = the front only, the behaviour before v337 (the lookahead also changes the dispatch order of the DXVK snapshot copies that share the dispatcher, so it is opt-in: set 4 together with `BltAsync`).
pub const LOOKAHEAD_DEFAULT: u32 = 1;

/// `BltLookahead` as the driver uses it: 1 is the old behaviour (the front only), 0 is read as 1,
/// anything above the maximum is cut to it.
pub const fn clamp_lookahead(raw: u32) -> usize {
    if raw <= 1 {
        1
    } else if raw as usize > LOOKAHEAD_MAX {
        LOOKAHEAD_MAX
    } else {
        raw as usize
    }
}

/// One entry of the ready queue's window, as the worker sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cand {
    /// The token names a live, undispatched request (a stale one neither dispatches nor blocks).
    pub live: bool,
    /// The destination resource.
    pub dst: u32,
    /// Admitted, producer boundary ready and destination writable: could be dispatched now.
    pub dispatchable: bool,
}

/// Which entry of the window the worker dispatches: the first one that can go, unless an EARLIER
/// live entry names the same destination (that one is older and has not gone, whatever its
/// reason: the later frame must not land first). With a window of one this is the old rule.
/// An entry that cannot go (its producer is slow, its destination is being read) no longer holds
/// the entries of unrelated destinations behind it.
/// `BltSupersede` default: 1 (on). 0 is the A/B lever (every queued Blt is copied in order).
pub const SUPERSEDE_DEFAULT: u32 = 1;

/// One queued windowed Blt as the supersede rule sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Queued {
    /// The destination resource (DWM's redirection surface of the window).
    pub dst: u32,
    /// The source's extent: a windowed Blt copies its whole source to the same place.
    pub src_extent: (u32, u32),
    /// SubmitCommand admitted it (its Present's DMA buffer exists and waits for the token).
    pub admitted: bool,
    /// The worker submitted its copy.
    pub dispatched: bool,
}

/// Whether `older` may complete without its copy because `newer`, presented after it, writes the
/// same pixels of the same destination: both admitted and not yet dispatched, the same nonzero
/// destination, the same nonzero source extent. Its Present then retires with the newer copy's
/// pixels on the destination, as a dropped frame would.
///
/// Why: a backlog of queued copies into one redirection surface keeps itself alive. Each copy may
/// go only when the destination is free (DWM's read of the previous frame retired), and while an
/// older request names the destination nothing newer can go first; an application with three
/// frames in flight refills the queue as fast as it drains, so once three are queued (a host
/// hiccup is enough: a screenshot that holds the viewer's buffers) every frame waits about three
/// composition periods (405.24: Heaven windowed held at ~96 fps, `BltAsyncInfl` 3 and
/// `BltDeferUs` ~31 ms per Blt, until a window move drained the queue). Dropping the superseded
/// copies drains it at once.
pub const fn superseded(older: Queued, newer: Queued) -> bool {
    older.dst != 0
        && older.dst == newer.dst
        && older.admitted
        && newer.admitted
        && !older.dispatched
        && !newer.dispatched
        && older.src_extent.0 != 0
        && older.src_extent.1 != 0
        && older.src_extent.0 == newer.src_extent.0
        && older.src_extent.1 == newer.src_extent.1
}

pub fn pick(window: &[Cand]) -> Option<usize> {
    let mut i = 0;
    while i < window.len() {
        let c = window[i];
        if c.live && c.dispatchable {
            let blocked = window[..i].iter().any(|e| e.live && e.dst == c.dst);
            if !blocked {
                return Some(i);
            }
        }
        i += 1;
    }
    None
}

/// The counters this feature writes, all in `kmd_render/src/ddi/blt_async.rs`. At most 14
/// characters and unique across `kmd_render` and `kmd_logic`; the indexed histograms are listed
/// out. A test below checks the list against the I/O file.
pub const COUNTERS: &[&str] = &[
    // Knobs in force.
    "BltAsyncKnob",
    "BltNoMirKnob",
    "BltVenusKnob",
    // The entry decision, made before everything else: Blts of the arm, Blts decided, Blts a
    // knob acts on, the last and every reason none did, and one counter per reason.
    "BltEntrySeen",
    "BltEntryDec",
    "BltEntryOk",
    "BltEntryWhy",
    "BltEntryMask",
    "BltNoEntryK",
    "BltNoEntryM",
    "BltNoEntryF",
    "BltNoEntryFc",
    "BltNoEntryS",
    "BltNoEntryO",
    // Asynchronous Blts: total, by route, current and peak in flight, failures, fallbacks.
    "BltAsyncN",
    "BltAsyncDir",
    "BltAsyncDefer",
    "BltAsyncInfl",
    "BltAsyncPk",
    "BltAsyncFail",
    "BltAsyncFall",
    "BltAsyncWhy",
    "BltAsyncMask",
    "BltAsyncBusy",
    "BltDeferUs",
    "BltDrainN",
    // A source still being read by an earlier copy when its next Present arrived.
    "BltSrcBusy",
    // Queued copies completed without a copy because a newer one into the same destination was
    // queued behind them (`BltSupersede`), and the knob in force.
    "BltSuperN",
    "BltSuperKnob",
    // Worker lookahead: knob in force, dispatches made ahead of a blocked front entry.
    "BltLookKnob",
    "BltLookN",
    // Submission to copy completion, histogram.
    "BltAsyncLat0",
    "BltAsyncLat1",
    "BltAsyncLat2",
    "BltAsyncLat3",
    "BltAsyncLat4",
    "BltAsyncLat5",
    "BltAsyncLat6",
    "BltAsyncLat7",
    // The DDI's CPU wait of the legacy arm (BltAsync 0): count, microseconds, histogram.
    "BltWaitN",
    "BltWaitUs",
    "BltWait0",
    "BltWait1",
    "BltWait2",
    "BltWait3",
    "BltWait4",
    "BltWait5",
    "BltWait6",
    "BltWait7",
    // The CPU mirror: done, skipped, microseconds; destination system copies marked invalid.
    "BltMirrorN",
    "BltMirrorSk",
    "BltMirrorUs",
    "BltNoMirInv",
    // `DxgkDdiPresent`'s own wall time: Blt arm (count, microseconds, maximum, histogram) and flip
    // arm (count, microseconds, maximum).
    "PrDdiBltN",
    "PrDdiBltUs",
    "PrDdiBltMax",
    "PrDdiBlt0",
    "PrDdiBlt1",
    "PrDdiBlt2",
    "PrDdiBlt3",
    "PrDdiBlt4",
    "PrDdiBlt5",
    "PrDdiBlt6",
    "PrDdiBlt7",
    "PrDdiFlipN",
    "PrDdiFlipUs",
    "PrDdiFlipMax",
];

#[cfg(test)]
mod tests {

    fn q(dst: u32, w: u32, admitted: bool, dispatched: bool) -> Queued {
        Queued { dst, src_extent: (w, 720), admitted, dispatched }
    }

    #[test]
    fn a_queued_copy_is_superseded_only_by_a_newer_full_copy_into_its_destination() {
        assert!(superseded(q(7, 1280, true, false), q(7, 1280, true, false)));
        // Another destination, another extent, an unadmitted or a dispatched request: never.
        assert!(!superseded(q(7, 1280, true, false), q(8, 1280, true, false)));
        assert!(!superseded(q(7, 1280, true, false), q(7, 1600, true, false)));
        assert!(!superseded(q(7, 1280, false, false), q(7, 1280, true, false)));
        assert!(!superseded(q(7, 1280, true, false), q(7, 1280, false, false)));
        assert!(!superseded(q(7, 1280, true, true), q(7, 1280, true, false)));
        assert!(!superseded(q(7, 1280, true, false), q(7, 1280, true, true)));
        // No identity or no extent: never.
        assert!(!superseded(q(0, 1280, true, false), q(0, 1280, true, false)));
        assert!(!superseded(q(7, 0, true, false), q(7, 0, true, false)));
        assert_eq!(SUPERSEDE_DEFAULT, 1);
    }

    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn facts() -> Facts {
        Facts {
            async_on: true,
            no_mirror_on: true,
            foreign_source: true,
            snapshot: false,
            dst_standard_buffer: true,
            boundary: Boundary::Live { ready: false },
            dst_deferred_pending: false,
            table_has_room: true,
        }
    }

    #[test]
    fn knob_off_is_always_the_legacy_arm() {
        for ready in [false, true] {
            let f = Facts {
                async_on: false,
                boundary: Boundary::Live { ready },
                ..facts()
            };
            assert_eq!(decide(f), Route::Legacy { why: Why::Off });
        }
    }

    #[test]
    fn only_foreign_sources_into_standard_buffers_are_asynchronous() {
        let f = Facts {
            foreign_source: false,
            ..facts()
        };
        assert_eq!(
            decide(f),
            Route::Legacy {
                why: Why::NotForeign
            }
        );
        let f = Facts {
            snapshot: true,
            ..facts()
        };
        assert_eq!(
            decide(f),
            Route::Legacy {
                why: Why::NotForeign
            }
        );
        let f = Facts {
            dst_standard_buffer: false,
            ..facts()
        };
        assert_eq!(
            decide(f),
            Route::Legacy {
                why: Why::NotBuffer
            }
        );
    }

    #[test]
    fn a_producer_that_has_not_finished_defers_the_copy() {
        // The copy must not start before the RM fence: never `Direct` while it is not ready,
        // whatever else is true.
        for no_mirror_on in [false, true] {
            for pending in [false, true] {
                for room in [false, true] {
                    let f = Facts {
                        no_mirror_on,
                        dst_deferred_pending: pending,
                        table_has_room: room,
                        boundary: Boundary::Live { ready: false },
                        ..facts()
                    };
                    assert_eq!(decide(f), Route::Deferred, "{f:?}");
                }
            }
        }
    }

    #[test]
    fn a_finished_producer_without_mirror_submits_directly() {
        let f = Facts {
            boundary: Boundary::Live { ready: true },
            ..facts()
        };
        assert_eq!(decide(f), Route::Direct);
    }

    #[test]
    fn a_finished_producer_with_mirror_goes_through_the_worker() {
        // The mirror is a PASSIVE CPU copy after the ring completion: only the deferred route has
        // a worker continuation for it.
        let f = Facts {
            no_mirror_on: false,
            boundary: Boundary::Live { ready: true },
            ..facts()
        };
        assert_eq!(decide(f), Route::Deferred);
    }

    #[test]
    fn a_direct_copy_never_overtakes_a_queued_one_for_the_same_destination() {
        let f = Facts {
            boundary: Boundary::Live { ready: true },
            dst_deferred_pending: true,
            ..facts()
        };
        assert_eq!(decide(f), Route::Deferred);
        let f = Facts {
            boundary: Boundary::None,
            dst_deferred_pending: true,
            ..facts()
        };
        assert_eq!(
            decide(f),
            Route::LegacyAfterDrain {
                why: Why::PendingNoBoundary
            }
        );
    }

    #[test]
    fn no_boundary_means_direct_only_without_the_mirror() {
        let f = Facts {
            boundary: Boundary::None,
            ..facts()
        };
        assert_eq!(decide(f), Route::Direct);
        let f = Facts {
            boundary: Boundary::None,
            no_mirror_on: false,
            ..facts()
        };
        assert_eq!(
            decide(f),
            Route::Legacy {
                why: Why::NoBoundaryMirror
            }
        );
    }

    #[test]
    fn a_dead_boundary_and_a_full_table_are_legacy_not_failures() {
        let f = Facts {
            boundary: Boundary::Dead,
            ..facts()
        };
        assert_eq!(
            decide(f),
            Route::Legacy {
                why: Why::BoundaryDead
            }
        );
        let f = Facts {
            boundary: Boundary::None,
            table_has_room: false,
            ..facts()
        };
        assert_eq!(
            decide(f),
            Route::Legacy {
                why: Why::TableFull
            }
        );
        // A ready producer with a full direct table still has the deferred route.
        let f = Facts {
            boundary: Boundary::Live { ready: true },
            table_has_room: false,
            ..facts()
        };
        assert_eq!(decide(f), Route::Deferred);
    }

    #[test]
    fn exhaustive_direct_is_never_chosen_without_its_preconditions() {
        // Direct needs: async on, foreign, no snapshot, standard buffer, no mirror, room, nothing
        // queued for the destination, and a boundary that is absent or finished.
        let bools = [false, true];
        let boundaries = [
            Boundary::None,
            Boundary::Dead,
            Boundary::Live { ready: false },
            Boundary::Live { ready: true },
        ];
        for &async_on in &bools {
            for &no_mirror_on in &bools {
                for &foreign_source in &bools {
                    for &snapshot in &bools {
                        for &dst_standard_buffer in &bools {
                            for &boundary in &boundaries {
                                for &dst_deferred_pending in &bools {
                                    for &table_has_room in &bools {
                                        let f = Facts {
                                            async_on,
                                            no_mirror_on,
                                            foreign_source,
                                            snapshot,
                                            dst_standard_buffer,
                                            boundary,
                                            dst_deferred_pending,
                                            table_has_room,
                                        };
                                        if decide(f) == Route::Direct {
                                            assert!(async_on && no_mirror_on, "{f:?}");
                                            assert!(foreign_source && !snapshot, "{f:?}");
                                            assert!(dst_standard_buffer, "{f:?}");
                                            assert!(table_has_room, "{f:?}");
                                            assert!(!dst_deferred_pending, "{f:?}");
                                            assert!(
                                                matches!(
                                                    boundary,
                                                    Boundary::None | Boundary::Live { ready: true }
                                                ),
                                                "{f:?}"
                                            );
                                        }
                                        if decide(f) == Route::Deferred {
                                            assert!(async_on, "{f:?}");
                                            assert!(
                                                matches!(boundary, Boundary::Live { .. }),
                                                "{f:?}"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn an_older_queued_copy_is_drained_whatever_the_mirror_says() {
        for no_mirror_on in [false, true] {
            for boundary in [Boundary::None, Boundary::Dead] {
                let f = Facts {
                    no_mirror_on,
                    boundary,
                    dst_deferred_pending: true,
                    ..facts()
                };
                assert!(
                    matches!(decide(f), Route::LegacyAfterDrain { .. }),
                    "{f:?} -> {:?}",
                    decide(f)
                );
            }
        }
    }

    #[test]
    fn a_plain_legacy_route_never_leaves_an_older_queued_copy_behind() {
        let bools = [false, true];
        let boundaries = [
            Boundary::None,
            Boundary::Dead,
            Boundary::Live { ready: false },
            Boundary::Live { ready: true },
        ];
        for &async_on in &bools {
            for &no_mirror_on in &bools {
                for &foreign_source in &bools {
                    for &snapshot in &bools {
                        for &dst_standard_buffer in &bools {
                            for &boundary in &boundaries {
                                for &room in &bools {
                                    let f = Facts {
                                        async_on,
                                        no_mirror_on,
                                        foreign_source,
                                        snapshot,
                                        dst_standard_buffer,
                                        boundary,
                                        dst_deferred_pending: true,
                                        table_has_room: room,
                                    };
                                    if let Route::Legacy { why } = decide(f) {
                                        // Only the Presents this feature never touches.
                                        assert!(
                                            matches!(
                                                why,
                                                Why::Off | Why::NotForeign | Why::NotBuffer
                                            ),
                                            "{f:?} -> Legacy({why:?})"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_level_5_edge_is_owed_exactly_where_no_other_stage_raises_it() {
        assert_eq!(edge_owed(Finish::Direct, true), Some(Edge::PresentBlt));
        assert_eq!(
            edge_owed(Finish::DeferredNoMirror, true),
            Some(Edge::WindowedBlt)
        );
        // The worker's mirror stage raises it for a mirrored copy.
        assert_eq!(edge_owed(Finish::DeferredMirrored, true), None);
        // A failed copy changed nothing on the screen.
        for f in [
            Finish::Direct,
            Finish::DeferredNoMirror,
            Finish::DeferredMirrored,
        ] {
            assert_eq!(edge_owed(f, false), None);
        }
    }

    #[test]
    fn lookahead_is_clamped() {
        assert_eq!(clamp_lookahead(0), 1);
        assert_eq!(clamp_lookahead(1), 1);
        assert_eq!(clamp_lookahead(LOOKAHEAD_DEFAULT), 1);
        assert_eq!(clamp_lookahead(8), 8);
        assert_eq!(clamp_lookahead(9), LOOKAHEAD_MAX);
        assert_eq!(clamp_lookahead(u32::MAX), LOOKAHEAD_MAX);
    }

    fn cand(dst: u32, dispatchable: bool) -> Cand {
        Cand {
            live: true,
            dst,
            dispatchable,
        }
    }

    #[test]
    fn a_blocked_front_does_not_hold_an_unrelated_destination() {
        // The front's producer is slow; the second entry is another window's frame.
        let w = [cand(1, false), cand(2, true)];
        assert_eq!(pick(&w), Some(1));
        // With a window of one it is the old behaviour: nothing.
        assert_eq!(pick(&w[..1]), None);
    }

    #[test]
    fn a_later_frame_never_overtakes_an_older_one_for_its_destination() {
        // Same destination: the older one cannot go, so the newer one waits behind it.
        let w = [cand(1, false), cand(1, true), cand(2, true)];
        assert_eq!(pick(&w), Some(2));
        let w = [cand(1, false), cand(1, true)];
        assert_eq!(pick(&w), None);
        // The older one can go: it goes first.
        let w = [cand(1, true), cand(1, true)];
        assert_eq!(pick(&w), Some(0));
    }

    #[test]
    fn a_stale_entry_neither_dispatches_nor_blocks() {
        let stale = Cand {
            live: false,
            dst: 1,
            dispatchable: true,
        };
        let w = [stale, cand(1, true)];
        assert_eq!(pick(&w), Some(1));
        assert_eq!(pick(&[stale]), None);
        assert_eq!(pick(&[]), None);
    }

    #[test]
    fn pick_keeps_per_destination_order_for_every_window() {
        // Exhaustive over 4 entries, 3 destinations, every dispatchable pattern: the pick is never
        // preceded by a live entry of its own destination, it is the first entry that could go,
        // and nothing is picked only when nothing could.
        let mut checked = 0u32;
        for code in 0..(3u32.pow(4) * 16) {
            let mut c = code;
            let mut w = [cand(0, false); 4];
            for slot in w.iter_mut() {
                slot.dst = 1 + c % 3;
                c /= 3;
            }
            for (i, slot) in w.iter_mut().enumerate() {
                slot.dispatchable = (c >> i) & 1 == 1;
            }
            let could_go = |j: usize| {
                let blocked = w[..j].iter().any(|e| e.dst == w[j].dst);
                w[j].dispatchable && !blocked
            };
            match pick(&w) {
                Some(k) => {
                    assert!(could_go(k), "{w:?} -> {k}");
                    for j in 0..k {
                        assert!(!could_go(j), "{w:?} skipped {j}");
                    }
                }
                None => {
                    for j in 0..4 {
                        assert!(!could_go(j), "{w:?} found nothing");
                    }
                }
            }
            checked += 1;
        }
        assert_eq!(checked, 81 * 16);
    }

    #[test]
    fn the_table_counts_readers_of_a_source_and_hands_tickets_back() {
        let mut t = Table::<4>::new();
        let ticket = LedgerTicket {
            slot: 3,
            resid: 77,
            generation: 5,
        };
        assert!(t.add(Entry::new(2, 9, 0).reading(77, ticket)));
        assert!(t.add(Entry::new(4, 9, 0).reading(77, LedgerTicket::NONE)));
        assert_eq!(t.readers(77), 2);
        assert_eq!(t.readers(78), 0);
        assert_eq!(t.readers(0), 0);
        let done = t.complete(2).unwrap();
        assert_eq!(done.source_id, 77);
        assert_eq!(done.ticket, ticket);
        assert_eq!(t.readers(77), 1);
        let oldest = t.pop_oldest().unwrap();
        assert_eq!(oldest.fence_id, 4);
        assert!(t.pop_oldest().is_none());
    }

    #[test]
    fn no_mirror_applies_only_to_a_foreign_source_into_a_buffer() {
        assert!(no_mirror_applies(true, true, false, true));
        assert!(!no_mirror_applies(false, true, false, true));
        assert!(!no_mirror_applies(true, false, false, true));
        assert!(!no_mirror_applies(true, true, true, true));
        assert!(!no_mirror_applies(true, true, false, false));
    }

    #[test]
    fn the_stale_mark_needs_a_backing() {
        assert!(stale_mark(true, true));
        assert!(!stale_mark(true, false));
        assert!(!stale_mark(false, true));
        assert!(!stale_mark(false, false));
    }

    #[test]
    fn buckets_are_monotonic_and_cover_everything() {
        assert_eq!(lat_bucket(0), 0);
        assert_eq!(lat_bucket(2_499), 0); // 249 us
        assert_eq!(lat_bucket(2_500), 1); // 250 us
        assert_eq!(lat_bucket(4_999), 1);
        assert_eq!(lat_bucket(5_000), 2);
        assert_eq!(lat_bucket(10_000), 3); // 1 ms
        assert_eq!(lat_bucket(20_000), 4); // 2 ms
        assert_eq!(lat_bucket(40_000), 5); // 4 ms
        assert_eq!(lat_bucket(80_000), 6); // 8 ms
        assert_eq!(lat_bucket(159_999), 6);
        assert_eq!(lat_bucket(160_000), 7); // 16 ms
        assert_eq!(lat_bucket(u64::MAX), 7);
        let mut last = 0;
        for t in (0..400_000u64).step_by(97) {
            let b = lat_bucket(t);
            assert!(b >= last && b < BUCKETS);
            last = b;
        }
    }

    #[test]
    fn microseconds_saturate_at_the_counter_width() {
        assert_eq!(us32(0), 0);
        assert_eq!(us32(10), 1);
        assert_eq!(us32(25), 2);
        assert_eq!(us32(u64::MAX), u32::MAX);
    }

    #[test]
    fn begin_joins_only_direct_writers() {
        assert_eq!(begin(Own::Free, 0), Begin::Acquire);
        // A free buffer with a stale count is still free: the count is the table's, and a free
        // buffer has no entry by construction.
        assert_eq!(begin(Own::Free, 3), Begin::Acquire);
        assert_eq!(begin(Own::Writer, 2), Begin::Overlap);
        // A writer that is not one of ours (a deferred copy, a CPU mirror).
        assert_eq!(begin(Own::Writer, 0), Begin::Busy);
        assert_eq!(begin(Own::Blocked, 0), Begin::Busy);
        assert_eq!(begin(Own::Blocked, 5), Begin::Busy);
    }

    #[test]
    fn the_last_retiring_writer_hands_the_buffer_back() {
        let mut t = Table::<4>::new();
        assert!(t.add(Entry::new(10, 7, 1)));
        assert!(t.add(Entry::new(12, 7, 2)));
        assert!(t.add(Entry::new(14, 9, 3)));
        assert_eq!(t.writers(7), 2);
        assert_eq!(t.writers(9), 1);
        assert_eq!(t.peak(), 3);
        let first = t.complete(10).unwrap();
        assert_eq!(first.resource_id, 7);
        assert!(!first.last_for_resource, "fence 12 still writes resource 7");
        let other = t.complete(14).unwrap();
        assert!(other.last_for_resource, "resource 9 had one writer");
        let second = t.complete(12).unwrap();
        assert!(second.last_for_resource);
        assert!(t.is_empty());
        assert_eq!(t.peak(), 3, "the peak is a statistic");
    }

    #[test]
    fn completion_may_come_in_any_order() {
        let mut t = Table::<4>::new();
        for (i, f) in [4u64, 6, 8].iter().enumerate() {
            assert!(t.add(Entry::new(*f, 1, i as u64)));
        }
        assert!(!t.complete(8).unwrap().last_for_resource);
        assert!(!t.complete(4).unwrap().last_for_resource);
        assert!(t.complete(6).unwrap().last_for_resource);
    }

    #[test]
    fn an_unknown_or_repeated_completion_is_nothing() {
        let mut t = Table::<2>::new();
        assert!(t.complete(5).is_none());
        assert!(t.add(Entry::new(5, 3, 0)));
        assert!(t.complete(6).is_none());
        assert!(t.complete(5).is_some());
        assert!(t.complete(5).is_none(), "a second completion finds nothing");
    }

    #[test]
    fn the_table_is_bounded_and_ordered() {
        let mut t = Table::<2>::new();
        assert!(t.add(Entry::new(3, 1, 0)));
        // Not above the newest fence.
        assert!(!t.add(Entry::new(3, 1, 0)));
        assert!(!t.add(Entry::new(2, 1, 0)));
        // Zero ids name nothing.
        assert!(!t.add(Entry::new(9, 0, 0)));
        assert!(!t.add(Entry::new(0, 1, 0)));
        assert!(t.add(Entry::new(4, 1, 0)));
        assert!(!t.has_room());
        assert!(!t.add(Entry::new(5, 1, 0)));
        assert_eq!(t.len(), 2);
        t.clear();
        assert!(t.is_empty() && t.has_room());
        assert_eq!(t.peak(), 2);
        // After a clear the fence order starts over (a new transport generation).
        assert!(t.add(Entry::new(1, 1, 0)));
    }

    fn ef() -> EntryFacts {
        EntryFacts {
            async_on: true,
            no_mirror_on: true,
            async_venus_on: false,
            source: SourceClass::Foreign,
            snapshot: false,
            dst_standard_buffer: true,
        }
    }

    #[test]
    fn entry_foreign_into_a_buffer_enters_with_both_knobs() {
        let e = entry(ef());
        assert_eq!(
            e,
            EntryDecision {
                async_enter: true,
                no_mirror: true,
                why: None
            }
        );
    }

    #[test]
    fn entry_knobs_are_independent() {
        let only_async = entry(EntryFacts {
            no_mirror_on: false,
            ..ef()
        });
        assert!(only_async.async_enter && !only_async.no_mirror && only_async.why.is_none());
        let only_mirror = entry(EntryFacts {
            async_on: false,
            ..ef()
        });
        assert!(!only_mirror.async_enter && only_mirror.no_mirror && only_mirror.why.is_none());
    }

    #[test]
    fn entry_both_knobs_off_is_knob_off_whatever_else_is_true() {
        for source in [
            SourceClass::Foreign,
            SourceClass::ForeignCopyOff,
            SourceClass::Venus,
        ] {
            for snapshot in [false, true] {
                for dst in [false, true] {
                    for venus in [false, true] {
                        let e = entry(EntryFacts {
                            async_on: false,
                            no_mirror_on: false,
                            async_venus_on: venus,
                            source,
                            snapshot,
                            dst_standard_buffer: dst,
                        });
                        assert_eq!(e.why, Some(EntryWhy::KnobOff));
                        assert!(!e.async_enter && !e.no_mirror);
                    }
                }
            }
        }
    }

    /// The hardware finding (v337.2, Heaven composed): a foreign source with `ForeignCopy` 0
    /// is not entered by either knob, and says so.
    #[test]
    fn entry_foreign_with_foreign_copy_off_is_refused_with_its_own_reason() {
        let e = entry(EntryFacts {
            source: SourceClass::ForeignCopyOff,
            async_venus_on: true,
            ..ef()
        });
        assert_eq!(e.why, Some(EntryWhy::ForeignCopyOff));
        assert!(!e.async_enter && !e.no_mirror);
    }

    #[test]
    fn entry_venus_source_needs_its_own_knob() {
        let off = entry(EntryFacts {
            source: SourceClass::Venus,
            ..ef()
        });
        assert_eq!(off.why, Some(EntryWhy::NotForeign));
        assert!(!off.async_enter && !off.no_mirror);
        let on = entry(EntryFacts {
            source: SourceClass::Venus,
            async_venus_on: true,
            ..ef()
        });
        assert!(on.async_enter && on.no_mirror && on.why.is_none());
    }

    #[test]
    fn entry_snapshot_and_image_destination_never_enter() {
        for source in [SourceClass::Foreign, SourceClass::Venus] {
            let snap = entry(EntryFacts {
                source,
                async_venus_on: true,
                snapshot: true,
                ..ef()
            });
            assert_eq!(snap.why, Some(EntryWhy::Snapshot));
            assert!(!snap.async_enter && !snap.no_mirror);
            let image = entry(EntryFacts {
                source,
                async_venus_on: true,
                dst_standard_buffer: false,
                ..ef()
            });
            assert_eq!(image.why, Some(EntryWhy::NotBuffer));
            assert!(!image.async_enter && !image.no_mirror);
        }
    }

    #[test]
    fn entry_reason_order_is_knob_snapshot_source_destination() {
        // Everything wrong at once: the knob row wins only when both knobs are off.
        let all_wrong = EntryFacts {
            async_on: true,
            no_mirror_on: false,
            async_venus_on: false,
            source: SourceClass::ForeignCopyOff,
            snapshot: true,
            dst_standard_buffer: false,
        };
        assert_eq!(entry(all_wrong).why, Some(EntryWhy::Snapshot));
        let no_snap = EntryFacts {
            snapshot: false,
            ..all_wrong
        };
        assert_eq!(entry(no_snap).why, Some(EntryWhy::ForeignCopyOff));
        let venus = EntryFacts {
            source: SourceClass::Venus,
            ..no_snap
        };
        assert_eq!(entry(venus).why, Some(EntryWhy::NotForeign));
        let venus_on = EntryFacts {
            async_venus_on: true,
            ..venus
        };
        assert_eq!(entry(venus_on).why, Some(EntryWhy::NotBuffer));
    }

    /// Exhaustive over every input: a knob acts iff it is on and nothing refuses, `why` is
    /// `None` exactly then, and `no_mirror` agrees with `no_mirror_applies` for foreign sources.
    #[test]
    fn entry_exhaustive_agrees_with_its_definition() {
        let classes = [
            SourceClass::Foreign,
            SourceClass::ForeignCopyOff,
            SourceClass::Venus,
        ];
        for bits in 0u32..32 {
            for source in classes {
                let f = EntryFacts {
                    async_on: bits & 1 != 0,
                    no_mirror_on: bits & 2 != 0,
                    async_venus_on: bits & 4 != 0,
                    snapshot: bits & 8 != 0,
                    dst_standard_buffer: bits & 16 != 0,
                    source,
                };
                let e = entry(f);
                let eligible = (f.async_on || f.no_mirror_on)
                    && !f.snapshot
                    && f.dst_standard_buffer
                    && match source {
                        SourceClass::Foreign => true,
                        SourceClass::ForeignCopyOff => false,
                        SourceClass::Venus => f.async_venus_on,
                    };
                assert_eq!(e.why.is_none(), eligible, "{f:?}");
                assert_eq!(e.async_enter, eligible && f.async_on, "{f:?}");
                assert_eq!(e.no_mirror, eligible && f.no_mirror_on, "{f:?}");
                if source == SourceClass::Foreign {
                    assert_eq!(
                        e.no_mirror,
                        no_mirror_applies(f.no_mirror_on, true, f.snapshot, f.dst_standard_buffer),
                        "{f:?}"
                    );
                }
                // The entry never admits what `decide` refuses for its own reasons.
                if e.async_enter {
                    let route = decide(Facts {
                        async_on: true,
                        no_mirror_on: f.no_mirror_on,
                        foreign_source: class_eligible(source, f.async_venus_on),
                        snapshot: false,
                        dst_standard_buffer: true,
                        boundary: Boundary::Live { ready: true },
                        dst_deferred_pending: false,
                        table_has_room: true,
                    });
                    assert!(!matches!(
                        route,
                        Route::Legacy {
                            why: Why::Off | Why::NotForeign | Why::NotBuffer
                        }
                    ));
                }
            }
        }
    }

    #[test]
    fn entry_why_codes_are_distinct_and_have_their_own_bit() {
        let all = [
            EntryWhy::KnobOff,
            EntryWhy::Snapshot,
            EntryWhy::NotForeign,
            EntryWhy::ForeignCopyOff,
            EntryWhy::NotBuffer,
            EntryWhy::Other,
        ];
        let mut mask = 0u32;
        for w in &all {
            assert_eq!(mask & w.bit(), 0, "{w:?} shares a bit");
            mask |= w.bit();
            assert!(w.code() >= 1 && w.code() <= 32);
        }
        assert_eq!(mask.count_ones() as usize, all.len());
    }

    #[test]
    fn a_venus_source_is_ordered_by_decide_like_a_foreign_one() {
        // With no boundary (nothing but the Venus ring orders a Venus source's copy) and the
        // mirror off the copy is direct; with the mirror on it needs the worker, so it falls back.
        let base = Facts {
            async_on: true,
            no_mirror_on: true,
            foreign_source: class_eligible(SourceClass::Venus, true),
            snapshot: false,
            dst_standard_buffer: true,
            boundary: Boundary::None,
            dst_deferred_pending: false,
            table_has_room: true,
        };
        assert_eq!(decide(base), Route::Direct);
        assert_eq!(
            decide(Facts {
                no_mirror_on: false,
                ..base
            }),
            Route::Legacy {
                why: Why::NoBoundaryMirror
            }
        );
    }

    #[test]
    fn why_codes_are_distinct_and_have_their_own_bit() {
        let all = [
            Why::Off,
            Why::NotForeign,
            Why::NotBuffer,
            Why::NoBoundaryMirror,
            Why::BoundaryDead,
            Why::QueueRefused,
            Why::TokenRefused,
            Why::SubmitRefused,
            Why::PendingNoBoundary,
            Why::TableFull,
            Why::DstBusy,
        ];
        let mut codes: Vec<u32> = all.iter().map(|w| w.code()).collect();
        let mut mask = 0u32;
        for w in &all {
            assert_eq!(mask & w.bit(), 0, "{w:?} shares a bit");
            mask |= w.bit();
        }
        codes.sort();
        codes.dedup();
        assert_eq!(codes.len(), all.len());
        assert!(codes.iter().all(|&c| c != 0 && c <= 32));
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        let mut names: Vec<&str> = COUNTERS.to_vec();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(!n.is_empty());
        }
        names.sort();
        let before = names.len();
        names.dedup();
        assert_eq!(names.len(), before, "duplicate counter name");
    }

    /// The sibling `kmd_render/src`, or `None` when this copy of the crate has none; with
    /// `HELIOS_REQUIRE_NAME_SCAN=1` an absent sibling fails the test instead of skipping it.
    fn render_src() -> Option<std::path::PathBuf> {
        let render = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../kmd_render/src");
        if render.exists() {
            return Some(render);
        }
        assert!(
            std::env::var("HELIOS_REQUIRE_NAME_SCAN").map_or(true, |v| v != "1"),
            "HELIOS_REQUIRE_NAME_SCAN=1 but {} does not exist: copy kmd_render next to kmd_logic",
            render.display()
        );
        None
    }

    fn literals(text: &str) -> Vec<std::string::String> {
        let mut out: Vec<std::string::String> = Vec::new();
        let mut rest = text;
        while let Some(i) = rest.find("b\"") {
            let tail = &rest[i + 2..];
            let Some(end) = tail.find('"') else {
                break;
            };
            let name = &tail[..end];
            if !name.is_empty()
                && name.chars().all(|c| c.is_ascii_alphanumeric())
                && !out.iter().any(|w| w == name)
            {
                out.push(name.into());
            }
            rest = &tail[end + 1..];
        }
        out
    }

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        let Some(render) = render_src() else {
            return;
        };
        let text = std::fs::read_to_string(render.join("ddi/blt_async.rs")).unwrap();
        let written = literals(&text);
        for n in COUNTERS {
            assert!(
                written.iter().any(|l| l == n),
                "{n} is listed but not written by ddi/blt_async.rs"
            );
        }
        for l in &written {
            // The knob names (read, not written) are the only other literals of the file.
            if l == "BltAsync" || l == "BltNoMirror" || l == "BltLookahead" || l == "BltAsyncVenus" {
                continue;
            }
            assert!(
                COUNTERS.contains(&l.as_str()),
                "{l} is written by ddi/blt_async.rs but not listed"
            );
        }
    }

    #[test]
    fn no_other_file_writes_these_names() {
        let Some(render) = render_src() else {
            return;
        };
        let mut stack = std::vec![render];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let name = p.file_name().unwrap().to_string_lossy().into_owned();
                    // The I/O file, and `diag.rs` which spells the two KNOB names.
                    if name == "blt_async.rs" && p.to_string_lossy().contains("/ddi/") {
                        continue;
                    }
                    if name == "diag.rs" {
                        continue;
                    }
                    checked += 1;
                    let text = std::fs::read_to_string(&p).unwrap();
                    assert!(
                        !text.contains("b\"Blt"),
                        "{} spells a counter or knob name starting Blt",
                        p.display()
                    );
                }
            }
        }
        assert!(checked > 20);
    }

    #[test]
    fn the_knob_names_are_spelled_in_diag_only() {
        let Some(render) = render_src() else {
            return;
        };
        let text = std::fs::read_to_string(render.join("diag.rs")).unwrap();
        assert!(text.contains("KnobName::new(b\"BltAsync\")"));
        assert!(text.contains("KnobName::new(b\"BltNoMirror\")"));
        assert!(text.contains("KnobName::new(b\"BltLookahead\")"));
        assert!(text.contains("KnobName::new(b\"BltAsyncVenus\")"));
        for n in COUNTERS {
            assert_ne!(*n, "BltAsync");
            assert_ne!(*n, "BltNoMirror");
            assert_ne!(*n, "BltAsyncVenus");
        }
    }
}
