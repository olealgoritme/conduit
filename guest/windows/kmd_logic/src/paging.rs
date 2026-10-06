//! Rules for `DxgkDdiBuildPagingBuffer`: which status the DDI may answer and how
//! a content transfer is bounded by what the driver knows of the allocation.
//!
//! # Legal statuses
//!
//! VidMm accepts exactly two answers from `BuildPagingBuffer`:
//! `STATUS_SUCCESS`, and `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` (the DMA
//! buffer was too small; VidMm retries with a bigger one). Anything else is
//! "Driver returned an invalid error code from BuildPagingBuffer" and bugchecks
//! `VIDEO_MEMORY_MANAGEMENT_INTERNAL` (0x10E, parameter 1 = 0xB): measured with
//! `STATUS_INSUFFICIENT_RESOURCES` (0xC000009A). This driver never emits DMA for
//! a content op, so the only status it may ever return is `STATUS_SUCCESS`; a
//! content operation it cannot perform moves nothing and says so through a
//! counter, never through the status.
//!
//! # Bounding a transfer
//!
//! A virtual transfer's `TransferSizeInBytes` was measured at 0x1E10000 for an
//! allocation the driver had recorded as 0x1C20000. The bytes beyond what the
//! driver knows of the allocation are treated as padding: the known part is
//! moved, the rest is not, and the operation still succeeds.

/// `STATUS_SUCCESS`.
pub const STATUS_SUCCESS: i32 = 0;
/// `STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER` (0xC01E0001).
pub const STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER: i32 = 0xC01E_0001u32 as i32;

/// Whether VidMm accepts `status` from `DxgkDdiBuildPagingBuffer`.
pub const fn is_legal_status(status: i32) -> bool {
    status == STATUS_SUCCESS || status == STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER
}

/// The status for a content operation the driver could not (or must not)
/// perform. Always legal; always `STATUS_SUCCESS`.
pub const CONTENT_SKIP_STATUS: i32 = STATUS_SUCCESS;

/// How much of a requested byte range the driver may move.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clamp {
    /// The whole request lies inside the known size.
    Full(u64),
    /// The request runs past the known size: move this many bytes (from the
    /// request's start) and treat the rest as padding.
    Clamped(u64),
    /// The request starts at or beyond the known size: move nothing.
    Nothing,
}

impl Clamp {
    /// Bytes to move.
    pub const fn len(self) -> u64 {
        match self {
            Clamp::Full(n) | Clamp::Clamped(n) => n,
            Clamp::Nothing => 0,
        }
    }

    pub const fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// Bound `requested` bytes at `offset` by the `known_size` of the allocation.
pub const fn clamp_range(known_size: u64, offset: u64, requested: u64) -> Clamp {
    match offset.checked_add(requested) {
        Some(end) if end <= known_size => Clamp::Full(requested),
        _ if offset >= known_size => Clamp::Nothing,
        _ => Clamp::Clamped(known_size - offset),
    }
}

/// Second bound, applied once the blob is mapped: the `moved` bytes at `offset`
/// (already cut to the recorded allocation size by [`clamp_range`]) may not
/// exceed the blob's MAPPED length either. A blob shorter than the recorded
/// allocation moves its known prefix instead of nothing at all.
pub const fn clamp_to_mapped(offset: u64, moved: u64, mapped_len: u64) -> Clamp {
    clamp_range(mapped_len, offset, moved)
}

// ── Skipped evictions: the "system copy invalid" ledger ─────────────────────
//
// Every failed content arm answers STATUS_SUCCESS (see above), so VidMm believes
// the operation ran. For a LOCAL_TO_SYSTEM eviction that means VidMm now thinks
// the SYSTEM pages hold the allocation's content, while they hold whatever they
// held before (garbage). The host blob is untouched and still authoritative —
// until a later SYSTEM_TO_LOCAL page-in copies those garbage pages OVER it. So a
// skipped eviction must remember itself, and the matching page-in must be
// skipped while the memory is set: the blob then stays at its last content, which
// is the one thing in the machine that is still right.

/// Capacity of the per-adapter ledger of allocations whose system copy is
/// invalid. Each entry needs a real eviction failure, so exhausting it is a
/// storm, not a workload; [`InvalidSet`] fails SAFE if it ever happens. Small on
/// purpose: it is stored inline in the adapter context, which is built by value
/// on the AddDevice stack.
pub const INVALID_SET_CAPACITY: usize = 64;

/// What [`InvalidSet::mark`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    /// Newly remembered.
    Newly,
    /// Already remembered (or the set is in overflow and remembers everything).
    Already,
    /// Not remembered because the set is full; it is now in overflow, which makes
    /// [`InvalidSet::contains`] answer `true` for every id until
    /// [`InvalidSet::clear_all`].
    Overflow,
    /// Resource id 0 names no allocation; nothing recorded.
    Ignored,
}

/// A small fixed-size set of resource ids whose system copy is invalid.
///
/// Fixed storage, no allocation: it lives under a spinlock. Full is not an
/// error that may be swallowed — forgetting an invalid allocation lets a page-in
/// overwrite a good blob with garbage — so a full set degrades to "every
/// allocation is suspect": page-ins are skipped (blobs stay authoritative, which
/// costs only system-resident CPU writes) until the next generation reset.
#[derive(Debug, Clone)]
pub struct InvalidSet<const N: usize> {
    ids: [u32; N],
    len: usize,
    overflow: bool,
    /// Ranges of marked allocations that SUCCESSFUL eviction chunks have copied
    /// since the mark (see [`InvalidSet::evict_chunk_done`]).
    cover: [CoverSlot; COVER_SLOTS],
}

/// Allocations whose partial-eviction coverage is tracked at once. Small on
/// purpose (the set is inline in the adapter context, built on a boot stack with
/// no headroom); an allocation that finds no free slot simply keeps its mark,
/// which is the safe answer.
pub const COVER_SLOTS: usize = 4;
/// Disjoint covered runs kept per allocation. VidMm's chunks of one eviction are
/// expected in ascending order and merge into ONE run; a second run absorbs a
/// chunk that arrived out of order. A third disjoint run restarts the tally.
pub const COVER_RUNS: usize = 2;

/// Coverage of one marked allocation: `runs[..n]` are disjoint, ascending,
/// non-adjacent `[start, end)` byte ranges. `id == 0` is a free slot.
#[derive(Debug, Clone, Copy)]
struct CoverSlot {
    id: u32,
    n: usize,
    runs: [(u64, u64); COVER_RUNS],
}

impl CoverSlot {
    const FREE: Self = Self {
        id: 0,
        n: 0,
        runs: [(0, 0); COVER_RUNS],
    };
}

/// What [`InvalidSet::evict_chunk_done`] concluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chunk {
    /// The allocation is not marked (or lost to overflow): nothing to do.
    NotMarked,
    /// The chunks that succeeded since the mark now cover the whole allocation:
    /// the mark was cleared.
    Revalidated,
    /// Still marked: part of the allocation has not (provably) been evicted since
    /// the mark. Always the safe answer.
    Pending,
}

impl<const N: usize> Default for InvalidSet<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> InvalidSet<N> {
    pub const fn new() -> Self {
        Self {
            ids: [0; N],
            len: 0,
            overflow: false,
            cover: [CoverSlot::FREE; COVER_SLOTS],
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0 && !self.overflow
    }

    pub fn is_overflowed(&self) -> bool {
        self.overflow
    }

    fn position(&self, id: u32) -> Option<usize> {
        self.ids[..self.len].iter().position(|&x| x == id)
    }

    /// Record that `id`'s system copy is invalid.
    ///
    /// A fresh failure also forgets what the earlier chunks covered: the skipped
    /// operation may have been any part of the allocation, and the system copy is
    /// only as good as its worst chunk.
    pub fn mark(&mut self, id: u32) -> Mark {
        if id == 0 {
            return Mark::Ignored;
        }
        self.drop_cover(id);
        if self.position(id).is_some() {
            return Mark::Already;
        }
        if self.len >= N {
            let was = self.overflow;
            self.overflow = true;
            return if was { Mark::Already } else { Mark::Overflow };
        }
        self.ids[self.len] = id;
        self.len += 1;
        Mark::Newly
    }

    /// Whether a page-in of `id` must be skipped.
    pub fn contains(&self, id: u32) -> bool {
        id != 0 && (self.overflow || self.position(id).is_some())
    }

    /// Whether a SYSTEM_TO_LOCAL page-in of `id` must be skipped. A page-in that
    /// is skipped leaves the blob (which the GPU may then write) as the only
    /// current copy, so any eviction coverage gathered so far is void.
    pub fn page_in_blocked(&mut self, id: u32) -> bool {
        let blocked = self.contains(id);
        if blocked {
            self.drop_cover(id);
        }
        blocked
    }

    /// Forget `id`. Returns whether it was recorded. Never lifts overflow: the
    /// ids lost to overflow are unknown.
    pub fn clear(&mut self, id: u32) -> bool {
        self.drop_cover(id);
        match self.position(id) {
            Some(i) => {
                self.len -= 1;
                self.ids[i] = self.ids[self.len];
                self.ids[self.len] = 0;
                true
            }
            None => false,
        }
    }

    /// A new transport generation: every id (and overflow) is meaningless.
    pub fn clear_all(&mut self) {
        self.ids = [0; N];
        self.len = 0;
        self.overflow = false;
        self.cover = [CoverSlot::FREE; COVER_SLOTS];
    }

    fn drop_cover(&mut self, id: u32) {
        if id == 0 {
            return;
        }
        for slot in self.cover.iter_mut() {
            if slot.id == id {
                *slot = CoverSlot::FREE;
            }
        }
    }

    /// A LOCAL_TO_SYSTEM eviction chunk of `[offset, offset + len)` of the
    /// `alloc_size`-byte allocation `id` just SUCCEEDED (`len` is the count
    /// actually moved). If `id` is marked invalid, this chunk joins the ranges
    /// that successful chunks copied since the mark; once their union is the whole
    /// allocation `[0, alloc_size)` the system copy is real again and the mark is
    /// cleared.
    ///
    /// This is what lets a large allocation that VidMm evicts in several chunks
    /// recover: [`eviction_revalidates`] alone clears only for a single chunk
    /// covering everything.
    ///
    /// Never optimistic. Anything that cannot be tracked (no free slot, a third
    /// disjoint run, a range that wraps) keeps the mark: the blob stays
    /// authoritative, and the cost is only CPU writes made while the allocation
    /// was system-resident. Coverage is voided by every later [`Self::mark`] of
    /// the same id (a chunk failed) and by every skipped page-in
    /// ([`Self::page_in_blocked`]), so it only ever describes ONE eviction pass.
    pub fn evict_chunk_done(&mut self, id: u32, alloc_size: u64, offset: u64, len: u64) -> Chunk {
        if id == 0 || self.position(id).is_none() {
            return Chunk::NotMarked;
        }
        if alloc_size == 0 || len == 0 || offset >= alloc_size {
            return Chunk::Pending;
        }
        // A count that wraps is not a count: claim nothing. One that merely runs
        // past the end (VidMm's padding) reaches the allocation's end.
        let Some(raw_end) = offset.checked_add(len) else {
            return Chunk::Pending;
        };
        let end = if raw_end < alloc_size {
            raw_end
        } else {
            alloc_size
        };
        if offset == 0 && end >= alloc_size {
            // One chunk did it: the classic whole-allocation eviction.
            self.clear(id);
            return Chunk::Revalidated;
        }
        // The slot of `id`, else a free one; none left keeps the mark.
        let at = match self.cover.iter().position(|c| c.id == id) {
            Some(i) => i,
            None => match self.cover.iter().position(|c| c.id == 0) {
                Some(i) => i,
                None => return Chunk::Pending,
            },
        };
        let slot = &mut self.cover[at];
        if slot.id != id {
            *slot = CoverSlot::FREE;
            slot.id = id;
        }
        // Existing runs plus this one, ascending by start, then merged where they
        // overlap or touch.
        let mut all = [(0u64, 0u64); COVER_RUNS + 1];
        all[..slot.n].copy_from_slice(&slot.runs[..slot.n]);
        all[slot.n] = (offset, end);
        let total = slot.n + 1;
        all[..total].sort_unstable();
        let mut merged = [(0u64, 0u64); COVER_RUNS + 1];
        let mut m = 0usize;
        for &(s, e) in &all[..total] {
            if m > 0 && s <= merged[m - 1].1 {
                if e > merged[m - 1].1 {
                    merged[m - 1].1 = e;
                }
            } else {
                merged[m] = (s, e);
                m += 1;
            }
        }
        if m > COVER_RUNS {
            // More pieces than are tracked: start over from this chunk alone.
            slot.n = 1;
            slot.runs = [(0, 0); COVER_RUNS];
            slot.runs[0] = (offset, end);
            return Chunk::Pending;
        }
        if m == 1 && merged[0].0 == 0 && merged[0].1 >= alloc_size {
            self.clear(id);
            return Chunk::Revalidated;
        }
        slot.n = m;
        slot.runs = [(0, 0); COVER_RUNS];
        slot.runs[..m].copy_from_slice(&merged[..m]);
        Chunk::Pending
    }
}

/// Whether a SUCCESSFUL eviction of `bytes` at `offset` makes the allocation's
/// whole system copy valid again, so the invalid mark may be dropped.
///
/// Only an eviction that covers the allocation from offset 0 does: a partial one
/// proves nothing about the pieces it did not move, and the mark may have been
/// set by one of them. `bytes` is the CLAMPED count actually moved.
pub const fn eviction_revalidates(alloc_size: u64, offset: u64, bytes: u64) -> bool {
    offset == 0 && bytes >= alloc_size
}

/// What a SYSTEM_TO_LOCAL page-in does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PageIn {
    /// Copy the system pages into the blob.
    Execute,
    /// The system pages are not the allocation's content (a skipped eviction):
    /// leave the blob alone. Counted, answered STATUS_SUCCESS.
    SkipBlobAuthoritative,
}

pub const fn page_in_decision(system_copy_invalid: bool) -> PageIn {
    if system_copy_invalid {
        PageIn::SkipBlobAuthoritative
    } else {
        PageIn::Execute
    }
}

// ── Transient mapping failures ──────────────────────────────────────────────

/// Attempts for a transient kernel mapping (`MmMapLockedPagesSpecifyCache`,
/// `MmMapIoSpace`, the blob's RESOURCE_MAP_BLOB) before the op is skipped.
pub const MAP_ATTEMPTS: u32 = 3;
/// Attempts when the failure was a host TIMEOUT: a retry of a host that is not
/// answering multiplies the stall (each attempt can wait for seconds), so one
/// retry is all it gets.
pub const MAP_TIMEOUT_ATTEMPTS: u32 = 2;

/// After `failed` consecutive failed attempts (1 = the first just failed): the
/// milliseconds to sleep before trying again, or `None` when the attempts are
/// spent. The sleeps are nominal (the kernel rounds a small relative timeout up
/// to the timer granularity, ~15.6 ms) and PASSIVE-only.
pub const fn retry_after_failure(failed: u32, timed_out: bool) -> Option<u64> {
    let max = if timed_out {
        MAP_TIMEOUT_ATTEMPTS
    } else {
        MAP_ATTEMPTS
    };
    if failed == 0 || failed >= max {
        return None;
    }
    Some(match failed {
        1 => 1,
        2 => 4,
        _ => 10,
    })
}

// ── Stale allocation contexts ───────────────────────────────────────────────

/// Whether an allocation context stamped with `ctx_serial` at creation belongs
/// to the transport generation that is up NOW (`current`, `None` with no
/// transport).
///
/// Resource ids restart at 1 in every transport generation, so an id from an
/// older generation can name a DIFFERENT live blob in this one. Serial 0 is the
/// "never stamped" value and is never current.
pub const fn alloc_is_current(ctx_serial: u64, current: Option<u64>) -> bool {
    match current {
        Some(now) => ctx_serial != 0 && ctx_serial == now,
        None => false,
    }
}

/// Row-count alignment for an external LINEAR image, in rows. EMPIRICAL (the
/// NVIDIA external-linear requirement rounds rows up to GOB granularity; 128 is
/// what the measurements produced).
pub const NV_LINEAR_ROW_ALIGN: u64 = 128;

/// Opaque tail slack an external LINEAR image requires beyond the padded rows.
/// Equally empirical.
pub const NV_LINEAR_TAIL_SLACK: u64 = 64 * 1024;

/// Size a blob that will be imported as an external LINEAR VkImage must have:
/// `pitch * align(height, 128) + 64 KiB`, never below one page. Deliberately
/// LARGER than `pitch * height`.
pub const fn linear_blob_size(pitch: u64, height: u64) -> u64 {
    let padded_rows = height.saturating_add(NV_LINEAR_ROW_ALIGN - 1) & !(NV_LINEAR_ROW_ALIGN - 1);
    let size = pitch
        .saturating_mul(padded_rows)
        .saturating_add(NV_LINEAR_TAIL_SLACK);
    if size < 4096 {
        4096
    } else {
        size
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_success_and_insufficient_dma_are_legal() {
        assert!(is_legal_status(STATUS_SUCCESS));
        assert!(is_legal_status(STATUS_GRAPHICS_INSUFFICIENT_DMA_BUFFER));
        // STATUS_INSUFFICIENT_RESOURCES: the status measured to bugcheck 0x10E/0xB.
        assert!(!is_legal_status(0xC000_009Au32 as i32));
        // STATUS_UNSUCCESSFUL, STATUS_NO_MEMORY, STATUS_INVALID_PARAMETER.
        assert!(!is_legal_status(0xC000_0001u32 as i32));
        assert!(!is_legal_status(0xC000_0017u32 as i32));
        assert!(!is_legal_status(0xC000_000Du32 as i32));
    }

    #[test]
    fn a_skipped_content_op_answers_a_legal_status() {
        assert!(is_legal_status(CONTENT_SKIP_STATUS));
    }

    #[test]
    fn clamp_inside_is_full() {
        assert_eq!(clamp_range(100, 0, 100), Clamp::Full(100));
        assert_eq!(clamp_range(100, 40, 60), Clamp::Full(60));
        assert_eq!(clamp_range(100, 100, 0), Clamp::Full(0));
        assert_eq!(clamp_range(100, 0, 0), Clamp::Full(0));
    }

    #[test]
    fn clamp_overrun_keeps_the_known_prefix() {
        assert_eq!(clamp_range(100, 0, 101), Clamp::Clamped(100));
        assert_eq!(clamp_range(100, 40, 61), Clamp::Clamped(60));
        assert_eq!(clamp_range(100, 99, 5), Clamp::Clamped(1));
    }

    #[test]
    fn clamp_beyond_the_allocation_moves_nothing() {
        assert_eq!(clamp_range(100, 100, 1), Clamp::Nothing);
        assert_eq!(clamp_range(100, 500, 10), Clamp::Nothing);
        assert_eq!(clamp_range(100, 101, 0), Clamp::Nothing);
        assert_eq!(clamp_range(0, 0, 1), Clamp::Nothing);
        assert!(clamp_range(100, 500, 10).is_empty());
    }

    #[test]
    fn clamp_survives_overflow() {
        assert_eq!(clamp_range(100, 10, u64::MAX), Clamp::Clamped(90));
        assert_eq!(clamp_range(100, u64::MAX, 2), Clamp::Nothing);
    }

    #[test]
    fn the_measured_5120x1440_transfer_is_clamped_not_refused() {
        // Recorded size 0x1C20000 (5120 x 1440 x 4), VidMm's VIRTUAL_TRANSFER
        // asked for 0x1E10000 at offset 0.
        let c = clamp_range(0x1C2_0000, 0, 0x1E1_0000);
        assert_eq!(c, Clamp::Clamped(0x1C2_0000));
        assert_eq!(c.len(), 29_491_200);
    }

    #[test]
    fn clamp_is_the_min_of_allocation_and_mapped_blob() {
        // Recorded 100 bytes, VidMm asks 100, the blob maps only 60.
        let first = clamp_range(100, 0, 100);
        assert_eq!(first, Clamp::Full(100));
        assert_eq!(clamp_to_mapped(0, first.len(), 60), Clamp::Clamped(60));
        // Offset inside the mapped part.
        assert_eq!(clamp_to_mapped(40, 50, 60), Clamp::Clamped(20));
        // Mapped longer than the recorded size: the first clamp already won.
        let first = clamp_range(100, 0, 150);
        assert_eq!(first, Clamp::Clamped(100));
        assert_eq!(clamp_to_mapped(0, first.len(), 4096), Clamp::Full(100));
        // Starts past the mapped length: nothing.
        assert_eq!(clamp_to_mapped(60, 10, 60), Clamp::Nothing);
        assert!(clamp_to_mapped(60, 10, 60).is_empty());
    }

    #[test]
    fn invalid_set_marks_and_clears_one_allocation() {
        let mut s = InvalidSet::<4>::new();
        assert!(s.is_empty());
        assert!(!s.contains(7));
        assert_eq!(s.mark(7), Mark::Newly);
        assert!(s.contains(7));
        assert!(!s.contains(8));
        assert_eq!(s.mark(7), Mark::Already);
        assert_eq!(s.len(), 1);
        assert!(s.clear(7));
        assert!(!s.contains(7));
        assert!(!s.clear(7));
        assert!(s.is_empty());
    }

    #[test]
    fn invalid_set_ignores_resource_zero() {
        let mut s = InvalidSet::<4>::new();
        assert_eq!(s.mark(0), Mark::Ignored);
        assert!(!s.contains(0));
        assert!(s.is_empty());
    }

    #[test]
    fn invalid_set_clear_keeps_the_others() {
        let mut s = InvalidSet::<4>::new();
        for id in [1, 2, 3] {
            assert_eq!(s.mark(id), Mark::Newly);
        }
        assert!(s.clear(1));
        assert!(!s.contains(1) && s.contains(2) && s.contains(3));
        assert_eq!(s.len(), 2);
        // The freed slot is reusable.
        assert_eq!(s.mark(9), Mark::Newly);
        assert!(s.contains(9));
    }

    #[test]
    fn a_full_invalid_set_fails_safe_for_every_allocation() {
        let mut s = InvalidSet::<2>::new();
        assert_eq!(s.mark(1), Mark::Newly);
        assert_eq!(s.mark(2), Mark::Newly);
        // Third cannot be recorded: remembering it is impossible, so EVERY
        // allocation reads as suspect and page-ins are skipped.
        assert_eq!(s.mark(3), Mark::Overflow);
        assert!(s.is_overflowed());
        assert!(s.contains(3));
        assert!(s.contains(99));
        assert_eq!(s.mark(4), Mark::Already);
        // Clearing a recorded id does not lift overflow: the lost ids are unknown.
        assert!(s.clear(1));
        assert!(s.contains(99));
        assert!(!s.is_empty());
        // A generation reset does.
        s.clear_all();
        assert!(s.is_empty());
        assert!(!s.contains(99) && !s.contains(2));
    }

    #[test]
    fn only_a_whole_allocation_eviction_revalidates() {
        assert!(eviction_revalidates(100, 0, 100));
        // A clamped transfer (VidMm asked for more than the recorded size) moved
        // the whole allocation: `bytes` is the clamped count.
        assert!(eviction_revalidates(0x1C2_0000, 0, 0x1C2_0000));
        assert!(!eviction_revalidates(100, 0, 99));
        assert!(!eviction_revalidates(100, 50, 50));
        assert!(!eviction_revalidates(100, 1, 100));
    }

    #[test]
    fn skipped_eviction_then_page_in_then_full_eviction() {
        // The data-loss sequence and its fix, as the DDI drives it.
        let mut s = InvalidSet::<8>::new();
        let id = 5;
        // 1. LOCAL_TO_SYSTEM could not run: remembered.
        assert_eq!(s.mark(id), Mark::Newly);
        // 2. SYSTEM_TO_LOCAL arrives: skipped, the blob stays.
        assert_eq!(
            page_in_decision(s.contains(id)),
            PageIn::SkipBlobAuthoritative
        );
        // 3. Still skipped on every further page-in.
        assert_eq!(
            page_in_decision(s.contains(id)),
            PageIn::SkipBlobAuthoritative
        );
        // 4. A later partial eviction does not lift it.
        if eviction_revalidates(4096, 0, 2048) {
            s.clear(id);
        }
        assert_eq!(
            page_in_decision(s.contains(id)),
            PageIn::SkipBlobAuthoritative
        );
        // 5. A whole-allocation eviction does: the system copy is real again.
        if eviction_revalidates(4096, 0, 4096) {
            s.clear(id);
        }
        assert_eq!(page_in_decision(s.contains(id)), PageIn::Execute);
    }

    const MIB: u64 = 1 << 20;

    #[test]
    fn chunks_in_order_revalidate_on_the_last_one() {
        let mut s = InvalidSet::<8>::new();
        let size = 8 * MIB;
        s.mark(5);
        for i in 0..3 {
            assert_eq!(
                s.evict_chunk_done(5, size, i * 2 * MIB, 2 * MIB),
                Chunk::Pending
            );
            assert!(s.contains(5));
        }
        assert_eq!(
            s.evict_chunk_done(5, size, 6 * MIB, 2 * MIB),
            Chunk::Revalidated
        );
        assert!(!s.contains(5));
        assert!(s.is_empty());
    }

    #[test]
    fn chunks_out_of_order_revalidate_too() {
        let mut s = InvalidSet::<8>::new();
        let size = 4 * MIB;
        s.mark(5);
        // Second half first, then the first half: two runs, then one.
        assert_eq!(
            s.evict_chunk_done(5, size, 2 * MIB, 2 * MIB),
            Chunk::Pending
        );
        assert_eq!(s.evict_chunk_done(5, size, 0, 2 * MIB), Chunk::Revalidated);
        assert!(!s.contains(5));
        // Middle, end, then start: two runs absorb it.
        s.mark(6);
        assert_eq!(s.evict_chunk_done(6, 3 * MIB, MIB, MIB), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(6, 3 * MIB, 2 * MIB, MIB), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(6, 3 * MIB, 0, MIB), Chunk::Revalidated);
        assert!(!s.contains(6));
    }

    #[test]
    fn overlapping_and_repeated_chunks_merge() {
        let mut s = InvalidSet::<8>::new();
        s.mark(5);
        assert_eq!(s.evict_chunk_done(5, 100, 0, 60), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(5, 100, 0, 60), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(5, 100, 40, 30), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(5, 100, 70, 30), Chunk::Revalidated);
    }

    #[test]
    fn a_gap_keeps_the_mark() {
        let mut s = InvalidSet::<8>::new();
        s.mark(5);
        assert_eq!(s.evict_chunk_done(5, 100, 0, 40), Chunk::Pending);
        // [40, 50) never moved.
        assert_eq!(s.evict_chunk_done(5, 100, 50, 50), Chunk::Pending);
        assert!(s.contains(5));
        assert_eq!(s.evict_chunk_done(5, 100, 40, 10), Chunk::Revalidated);
    }

    #[test]
    fn a_failed_chunk_voids_the_coverage() {
        let mut s = InvalidSet::<8>::new();
        s.mark(5);
        assert_eq!(s.evict_chunk_done(5, 100, 0, 50), Chunk::Pending);
        // The next chunk is skipped: the system copy is bad again, whichever part
        // it was, and what the first chunk covered cannot complete it.
        assert_eq!(s.mark(5), Mark::Already);
        assert_eq!(s.evict_chunk_done(5, 100, 50, 50), Chunk::Pending);
        assert!(s.contains(5));
        // Only a fresh complete pass clears it.
        assert_eq!(s.evict_chunk_done(5, 100, 0, 50), Chunk::Revalidated);
    }

    #[test]
    fn a_skipped_page_in_voids_the_coverage() {
        let mut s = InvalidSet::<8>::new();
        s.mark(5);
        assert_eq!(s.evict_chunk_done(5, 100, 0, 50), Chunk::Pending);
        // The page-in is skipped: the blob is now the live copy and the GPU may
        // write it, so the earlier chunk's bytes are stale.
        assert!(s.page_in_blocked(5));
        assert_eq!(s.evict_chunk_done(5, 100, 50, 50), Chunk::Pending);
        assert!(s.contains(5));
        // A page-in of an allocation that is not marked changes nothing.
        assert!(!s.page_in_blocked(9));
    }

    #[test]
    fn an_unmarked_allocation_is_left_alone() {
        let mut s = InvalidSet::<8>::new();
        assert_eq!(s.evict_chunk_done(5, 100, 0, 50), Chunk::NotMarked);
        assert_eq!(s.evict_chunk_done(0, 100, 0, 100), Chunk::NotMarked);
        // No slot was spent on it: every slot is still free for marked ones.
        for id in 1..=COVER_SLOTS as u32 {
            s.mark(id);
            assert_eq!(s.evict_chunk_done(id, 100, 0, 10), Chunk::Pending);
        }
    }

    #[test]
    fn a_whole_allocation_chunk_revalidates_without_a_slot() {
        let mut s = InvalidSet::<8>::new();
        // Fill every slot with other allocations.
        for id in 1..=COVER_SLOTS as u32 {
            s.mark(id);
            assert_eq!(s.evict_chunk_done(id, 100, 0, 10), Chunk::Pending);
        }
        s.mark(40);
        // No slot for a partial chunk: the mark stays (safe).
        assert_eq!(s.evict_chunk_done(40, 100, 0, 50), Chunk::Pending);
        assert!(s.contains(40));
        // The classic single whole-allocation eviction needs no slot.
        assert_eq!(s.evict_chunk_done(40, 100, 0, 100), Chunk::Revalidated);
        assert!(!s.contains(40));
        // A clamped count past the end (VidMm's padding) still counts as whole.
        s.mark(41);
        assert_eq!(s.evict_chunk_done(41, 100, 0, 150), Chunk::Revalidated);
    }

    #[test]
    fn a_third_disjoint_run_restarts_the_tally_and_keeps_the_mark() {
        let mut s = InvalidSet::<8>::new();
        s.mark(5);
        assert_eq!(s.evict_chunk_done(5, 100, 0, 10), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(5, 100, 20, 10), Chunk::Pending);
        // Third disjoint run: more pieces than tracked.
        assert_eq!(s.evict_chunk_done(5, 100, 40, 10), Chunk::Pending);
        assert!(s.contains(5));
        // The restart kept only [40, 50): the rest has to arrive again.
        assert_eq!(s.evict_chunk_done(5, 100, 0, 40), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(5, 100, 50, 50), Chunk::Revalidated);
    }

    #[test]
    fn a_chunk_past_the_allocation_or_empty_or_wrapping_changes_nothing() {
        let mut s = InvalidSet::<8>::new();
        s.mark(5);
        assert_eq!(s.evict_chunk_done(5, 100, 100, 10), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(5, 100, 500, 10), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(5, 100, 10, 0), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(5, 0, 0, 10), Chunk::Pending);
        // offset + len wraps: never panics and claims nothing.
        assert_eq!(s.evict_chunk_done(5, 100, 10, u64::MAX), Chunk::Pending);
        assert!(s.contains(5));
        // None of the above recorded anything: [10, 100) is still missing.
        assert_eq!(s.evict_chunk_done(5, 100, 0, 10), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(5, 100, 10, 90), Chunk::Revalidated);
    }

    #[test]
    fn destroy_discard_and_generation_reset_drop_the_coverage() {
        let mut s = InvalidSet::<8>::new();
        s.mark(5);
        assert_eq!(s.evict_chunk_done(5, 100, 0, 50), Chunk::Pending);
        // DISCARD_CONTENT / destroy.
        assert!(s.clear(5));
        // Reused id after a re-mark starts from nothing.
        s.mark(5);
        assert_eq!(s.evict_chunk_done(5, 100, 50, 50), Chunk::Pending);
        assert_eq!(s.evict_chunk_done(5, 100, 0, 50), Chunk::Revalidated);
        // New generation.
        s.mark(6);
        assert_eq!(s.evict_chunk_done(6, 100, 0, 50), Chunk::Pending);
        s.clear_all();
        s.mark(6);
        assert_eq!(s.evict_chunk_done(6, 100, 50, 50), Chunk::Pending);
    }

    #[test]
    fn an_overflowed_set_is_never_revalidated_by_chunks() {
        let mut s = InvalidSet::<2>::new();
        s.mark(1);
        s.mark(2);
        assert_eq!(s.mark(3), Mark::Overflow);
        // 3 was lost to overflow: it is not individually marked, so no chunk of
        // it can lift anything, and overflow itself stays until clear_all.
        assert_eq!(s.evict_chunk_done(3, 100, 0, 100), Chunk::NotMarked);
        assert!(s.contains(3));
    }

    #[test]
    fn discard_and_destroy_drop_the_mark() {
        let mut s = InvalidSet::<8>::new();
        s.mark(5);
        s.mark(6);
        // DISCARD_CONTENT / DestroyAllocation of 5.
        assert!(s.clear(5));
        assert_eq!(page_in_decision(s.contains(5)), PageIn::Execute);
        assert_eq!(
            page_in_decision(s.contains(6)),
            PageIn::SkipBlobAuthoritative
        );
    }

    #[test]
    fn no_flag_means_page_in_executes() {
        assert_eq!(page_in_decision(false), PageIn::Execute);
    }

    #[test]
    fn map_retries_are_bounded_and_back_off() {
        assert_eq!(retry_after_failure(1, false), Some(1));
        assert_eq!(retry_after_failure(2, false), Some(4));
        assert_eq!(retry_after_failure(3, false), None);
        assert_eq!(retry_after_failure(9, false), None);
        // Zero failures is not a failure: nothing to retry.
        assert_eq!(retry_after_failure(0, false), None);
        // The longest sleep stays inside the 1-10 ms the caller documents.
        for failed in 1..MAP_ATTEMPTS {
            let ms = retry_after_failure(failed, false).unwrap();
            assert!((1..=10).contains(&ms));
        }
    }

    #[test]
    fn a_host_timeout_is_retried_once_only() {
        assert_eq!(retry_after_failure(1, true), Some(1));
        assert_eq!(retry_after_failure(2, true), None);
    }

    #[test]
    fn allocation_context_serial_must_match_the_live_generation() {
        assert!(alloc_is_current(3, Some(3)));
        // Created in an older generation: its resource ids mean something else now.
        assert!(!alloc_is_current(2, Some(3)));
        assert!(!alloc_is_current(4, Some(3)));
        // No transport at all (between StopDevice and StartDevice).
        assert!(!alloc_is_current(3, None));
        // Never stamped.
        assert!(!alloc_is_current(0, Some(0)));
        assert!(!alloc_is_current(0, Some(1)));
        assert!(!alloc_is_current(0, None));
    }

    #[test]
    fn linear_blob_size_5120x1440_vector() {
        // Pitch 20480 (5120 x 4). Tight size is 0x1C20000; the guess pads the
        // 1440 rows to 1536 and adds 64 KiB: 0x1E10000, the number VidMm's
        // transfer carried.
        assert_eq!(20480u64 * 1440, 0x1C2_0000);
        assert_eq!(linear_blob_size(20480, 1440), 0x1E1_0000);
        assert_eq!(linear_blob_size(20480, 1440), 20480 * 1536 + 0x1_0000);
    }

    #[test]
    fn linear_blob_size_measured_vector_and_floor() {
        // 1024x1872 measured 7864320 = pitch * align(1872, 128); the guess is
        // deliberately larger (tail slack).
        assert_eq!(4096 * 1920, 7_864_320);
        assert_eq!(linear_blob_size(4096, 1872), 7_864_320 + 0x1_0000);
        assert!(linear_blob_size(0, 0) >= 4096);
        assert_eq!(linear_blob_size(u64::MAX, u64::MAX), u64::MAX);
    }
}
