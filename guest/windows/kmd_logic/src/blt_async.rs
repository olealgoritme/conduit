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
    /// The source is an adopted foreign resource (the KMD's own record, never the creator's word).
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

/// The route of one Blt.
///
/// The table (rows are tried top down):
///
/// | condition | route |
/// |---|---|
/// | `BltAsync` 0 | legacy |
/// | not a foreign source, or a snapshot | legacy |
/// | destination is not a standard buffer | legacy |
/// | no boundary, mirror on | legacy (the mirror needs the worker, the worker needs a boundary) |
/// | no boundary, mirror off, a deferred request for the destination is queued | legacy after drain |
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
            if !f.no_mirror_on {
                Route::Legacy {
                    why: Why::NoBoundaryMirror,
                }
            } else if f.dst_deferred_pending {
                Route::LegacyAfterDrain {
                    why: Why::PendingNoBoundary,
                }
            } else if !f.table_has_room {
                Route::Legacy {
                    why: Why::TableFull,
                }
            } else {
                Route::Direct
            }
        }
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
}

/// What a retired entry hands back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Done {
    pub resource_id: u32,
    pub t0: u64,
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

const EMPTY: Entry = Entry {
    fence_id: 0,
    resource_id: 0,
    t0: 0,
};

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
            last_for_resource: self.writers(entry.resource_id) == 0,
        })
    }

    /// Forget everything (a new transport generation). The peak survives: it is a statistic.
    pub fn clear(&mut self) {
        self.entries = [EMPTY; N];
        self.len = 0;
    }
}

/// The counters this feature writes, all in `kmd_render/src/ddi/blt_async.rs`. At most 14
/// characters and unique across `kmd_render` and `kmd_logic`; the indexed histograms are listed
/// out. A test below checks the list against the I/O file.
pub const COUNTERS: &[&str] = &[
    // Knobs in force.
    "BltAsyncKnob",
    "BltNoMirKnob",
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
];

#[cfg(test)]
mod tests {
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
        assert!(t.add(Entry {
            fence_id: 10,
            resource_id: 7,
            t0: 1
        }));
        assert!(t.add(Entry {
            fence_id: 12,
            resource_id: 7,
            t0: 2
        }));
        assert!(t.add(Entry {
            fence_id: 14,
            resource_id: 9,
            t0: 3
        }));
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
            assert!(t.add(Entry {
                fence_id: *f,
                resource_id: 1,
                t0: i as u64
            }));
        }
        assert!(!t.complete(8).unwrap().last_for_resource);
        assert!(!t.complete(4).unwrap().last_for_resource);
        assert!(t.complete(6).unwrap().last_for_resource);
    }

    #[test]
    fn an_unknown_or_repeated_completion_is_nothing() {
        let mut t = Table::<2>::new();
        assert!(t.complete(5).is_none());
        assert!(t.add(Entry {
            fence_id: 5,
            resource_id: 3,
            t0: 0
        }));
        assert!(t.complete(6).is_none());
        assert!(t.complete(5).is_some());
        assert!(t.complete(5).is_none(), "a second completion finds nothing");
    }

    #[test]
    fn the_table_is_bounded_and_ordered() {
        let mut t = Table::<2>::new();
        assert!(t.add(Entry {
            fence_id: 3,
            resource_id: 1,
            t0: 0
        }));
        // Not above the newest fence.
        assert!(!t.add(Entry {
            fence_id: 3,
            resource_id: 1,
            t0: 0
        }));
        assert!(!t.add(Entry {
            fence_id: 2,
            resource_id: 1,
            t0: 0
        }));
        // Zero ids name nothing.
        assert!(!t.add(Entry {
            fence_id: 9,
            resource_id: 0,
            t0: 0
        }));
        assert!(!t.add(Entry {
            fence_id: 0,
            resource_id: 1,
            t0: 0
        }));
        assert!(t.add(Entry {
            fence_id: 4,
            resource_id: 1,
            t0: 0
        }));
        assert!(!t.has_room());
        assert!(!t.add(Entry {
            fence_id: 5,
            resource_id: 1,
            t0: 0
        }));
        assert_eq!(t.len(), 2);
        t.clear();
        assert!(t.is_empty() && t.has_room());
        assert_eq!(t.peak(), 2);
        // After a clear the fence order starts over (a new transport generation).
        assert!(t.add(Entry {
            fence_id: 1,
            resource_id: 1,
            t0: 0
        }));
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
            if l == "BltAsync" || l == "BltNoMirror" {
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
        for n in COUNTERS {
            assert_ne!(*n, "BltAsync");
            assert_ne!(*n, "BltNoMirror");
        }
    }
}
