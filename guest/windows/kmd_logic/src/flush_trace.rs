//! The flush gate's timeline (`HEFL`, `guest/windows/docs/flush-gate.md` section 9).
//!
//! The gate makes ONE promise: a D3D11 flush's WDDM fence retires when the flush's GPU
//! work is done. Whether that holds, and when each step of it happens, is invisible
//! without a debugger: the Render, the SubmitCommand of the DMA buffer that carries it
//! and the DPC that retires its fence run in three contexts at three IRQLs. This module
//! is the recorder for those three points and the pure rules that read the result.
//!
//! * [`Ring`]: a fixed ring of [`RING_LEN`] events, written with atomics only (no
//!   allocation, no lock, no registry), so the DPC and DISPATCH writers may use it. The
//!   caller supplies the interrupt-time stamp (100 ns); this crate reads no clock.
//! * [`submit_matches`]: did SubmitCommand decode the boundary the Render merged?
//! * [`lag_us`]: 100 ns stamps to microseconds.
//! * [`verdict`]: the counters of one run, reduced to the cause they support, so the
//!   host session reads one registry value (`FlGVerdict`) instead of a table.
//!
//! Everything here is a function of its arguments, host-tested.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

/// Slots in the ring (a power of two: the index is one mask).
pub const RING_LEN: usize = 64;
const MASK: u32 = RING_LEN as u32 - 1;

/// Event kinds.
pub mod kind {
    /// `DxgkDdiRender` resolved a `HEFL` record (PASSIVE).
    pub const RENDER: u8 = 1;
    /// `DxgkDdiSubmitCommand` saw a DMA buffer that carries a `HEFL` Render (DISPATCH).
    pub const SUBMIT: u8 = 2;
    /// The WDDM fence of such a buffer was delivered to dxgkrnl (DPC, or SubmitCommand
    /// itself when it was already satisfiable).
    pub const RETIRE: u8 = 3;
}

/// Event flags. The meaning depends on the kind; each group is documented with it.
pub mod flag {
    // RENDER
    /// The record asked for and got a stream point.
    pub const STREAM: u8 = 1;
    /// The record asked for and got an RM fence.
    pub const FENCE: u8 = 2;
    /// The packet has no boundary of its own (wire rung, degrade, merge error).
    pub const WIRE: u8 = 4;
    /// The record asked for something it did not get (counted `FlGDeg`).
    pub const DEGRADED: u8 = 8;
    /// The packet was stamped with an explicit wire floor.
    pub const STAMPED: u8 = 16;
    /// A stream point of value 0: "already complete", waits for nothing.
    pub const ZERO_POINT: u8 = 32;
    /// A render that found an earlier `HEFL` Render of the same context still waiting
    /// for its SubmitCommand (batched into one DMA buffer, or never submitted).
    pub const BATCHED: u8 = 64;
    // SUBMIT
    /// The boundary SubmitCommand decoded is the one the Render merged ([`submit_matches`]).
    pub const MATCH: u8 = 1;
    /// The fence was satisfiable at SubmitCommand and completed there, never queued.
    pub const IMMEDIATE: u8 = 2;
    // RETIRE
    /// The fence retired after the `WddmHeadMs` rebase: its boundary was replaced.
    pub const REBASED: u8 = 1;
    /// No SUBMIT event of this fence is left in the ring: the lag is unknown (0).
    pub const NO_SUBMIT: u8 = 2;
}

/// One recorded event.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Event {
    /// 1-based position in the ring's history.
    pub seq: u32,
    pub kind: u8,
    pub flags: u8,
    /// Low 32 bits of the context handle (a pointer: enough to tell contexts apart).
    pub ctx_low: u32,
    /// RENDER: 0. SUBMIT / RETIRE: the WDDM `SubmissionFenceId`.
    pub fence: u32,
    /// RENDER: the stream point asked for. SUBMIT: microseconds since the Render.
    /// RETIRE: microseconds since the SUBMIT.
    pub aux: u32,
    /// The boundary: a tagged stream/gate boundary (bit 63 set), or a wire fence id
    /// (the stamped floor on RENDER, the decoded `gpu_fence_id` on SUBMIT), or 0.
    pub boundary: u64,
    /// Interrupt time, 100 ns units.
    pub stamp_100ns: u64,
}

struct Slot {
    /// 0 while being written (or never written), else the event's `seq`.
    commit: AtomicU32,
    /// `kind | flags << 8 | ctx_low << 16`.
    meta: AtomicU64,
    /// `fence | aux << 32`.
    fence_aux: AtomicU64,
    boundary: AtomicU64,
    stamp: AtomicU64,
}

impl Slot {
    const fn new() -> Self {
        Self {
            commit: AtomicU32::new(0),
            meta: AtomicU64::new(0),
            fence_aux: AtomicU64::new(0),
            boundary: AtomicU64::new(0),
            stamp: AtomicU64::new(0),
        }
    }
}

/// The ring. `const`-constructible, so a `static` needs no initialiser at run time.
pub struct Ring {
    head: AtomicU32,
    slots: [Slot; RING_LEN],
}

impl Ring {
    pub const fn new() -> Self {
        Self {
            head: AtomicU32::new(0),
            slots: [const { Slot::new() }; RING_LEN],
        }
    }

    /// Events recorded so far (the `seq` of the newest one). Wraps at 2^32, which at
    /// this event rate is not a concern for a diagnostic.
    pub fn cursor(&self) -> u32 {
        self.head.load(Ordering::Acquire)
    }

    /// Append one event; returns its `seq`. Atomics only.
    #[allow(clippy::too_many_arguments)]
    pub fn record(
        &self,
        kind: u8,
        flags: u8,
        ctx_low: u32,
        fence: u32,
        aux: u32,
        boundary: u64,
        stamp_100ns: u64,
    ) -> u32 {
        // 0 is the "being written / empty" marker of `commit`; the counter reaches it
        // again only after 2^32 events, and that one event is then simply unreadable.
        let seq = self.head.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        let slot = &self.slots[(seq.wrapping_sub(1) & MASK) as usize];
        // Invalidate the previous occupant before touching the payload, so a reader
        // that started on it fails its final check instead of mixing two events.
        slot.commit.store(0, Ordering::Release);
        slot.meta.store(
            kind as u64 | (flags as u64) << 8 | (ctx_low as u64) << 16,
            Ordering::Relaxed,
        );
        slot.fence_aux
            .store(fence as u64 | (aux as u64) << 32, Ordering::Relaxed);
        slot.boundary.store(boundary, Ordering::Relaxed);
        slot.stamp.store(stamp_100ns, Ordering::Relaxed);
        slot.commit.store(seq, Ordering::Release);
        seq
    }

    /// The event with this `seq`, if it is still retained and was not overwritten
    /// while it was copied.
    pub fn read(&self, seq: u32) -> Option<Event> {
        let head = self.cursor();
        // Not yet recorded, or recorded and already overwritten.
        if seq == 0 || seq > head || head - seq >= RING_LEN as u32 {
            return None;
        }
        let slot = &self.slots[(seq.wrapping_sub(1) & MASK) as usize];
        if slot.commit.load(Ordering::Acquire) != seq {
            return None;
        }
        let meta = slot.meta.load(Ordering::Relaxed);
        let fence_aux = slot.fence_aux.load(Ordering::Relaxed);
        let event = Event {
            seq,
            kind: meta as u8,
            flags: (meta >> 8) as u8,
            ctx_low: (meta >> 16) as u32,
            fence: fence_aux as u32,
            aux: (fence_aux >> 32) as u32,
            boundary: slot.boundary.load(Ordering::Relaxed),
            stamp_100ns: slot.stamp.load(Ordering::Relaxed),
        };
        (slot.commit.load(Ordering::Acquire) == seq).then_some(event)
    }

    /// The newest retained event of `kind` for `fence`, scanning newest to oldest.
    pub fn latest_for_fence(&self, kind: u8, fence: u32) -> Option<Event> {
        let head = self.cursor();
        let mut back = 0u32;
        while back < RING_LEN as u32 && back < head {
            if let Some(event) = self.read(head - back) {
                if event.kind == kind && event.fence == fence {
                    return Some(event);
                }
            }
            back += 1;
        }
        None
    }
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

/// Longest the synchronous gate (`FlGSyncMs`) may hold a `DxgkDdiRender`, in
/// milliseconds. The knob is clamped to it in code: a typo must not be able to wedge an
/// application's flush for minutes.
pub const SYNC_MS_MAX: u32 = 2000;

/// The `FlGSyncMs` value the driver uses: 0 stays 0 (off), anything else is capped.
pub const fn clamp_sync_ms(requested: u32) -> u32 {
    if requested > SYNC_MS_MAX {
        SYNC_MS_MAX
    } else {
        requested
    }
}

/// Microseconds between two interrupt-time stamps (100 ns units), saturating: a clock
/// that stepped back reads 0, and the result never wraps a `u32` (71 minutes).
pub const fn lag_us(from_100ns: u64, to_100ns: u64) -> u32 {
    let us = to_100ns.saturating_sub(from_100ns) / 10;
    if us > u32::MAX as u64 {
        u32::MAX
    } else {
        us as u32
    }
}

/// Whether SubmitCommand decoded what the Render merged.
///
/// * `expected_boundary != 0`: the record must carry a boundary of the same handle at
///   or above the requested value (the merge keeps the larger of one handle; a requested
///   value of 0 waits for nothing, so any record satisfies it): `flush_gate::boundary_kept`.
/// * else `expected_floor != 0` (the wire floor the Render stamped): the record's
///   `gpu_fence_id` must be at least the floor (the merge keeps the larger id).
/// * else nothing was expected (a transport generation that had issued no fence): a
///   record is a match whatever it holds.
///
/// `decoded_*` are 0 when SubmitCommand found no record at all.
pub fn submit_matches(
    expected_boundary: u64,
    expected_floor: u64,
    decoded_stream_boundary: u64,
    decoded_gpu_fence: u64,
) -> bool {
    if expected_boundary != 0 {
        return crate::flush_gate::boundary_kept(expected_boundary, decoded_stream_boundary);
    }
    if expected_floor != 0 {
        return decoded_gpu_fence >= expected_floor;
    }
    true
}

/// The counters of one run (`publish_flush_gate_counters`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    /// `HEFL` records resolved at Render (`FlGRec`).
    pub render: u32,
    /// DMA buffers that reached SubmitCommand carrying a `HEFL` Render (`FlGSub`).
    pub submit: u32,
    /// Renders that found an earlier one of the same context still waiting for its
    /// SubmitCommand (`FlGBat`): batched into one DMA buffer, or never submitted.
    pub batched: u32,
    /// SubmitCommands whose decoded boundary was the merged one (`FlGMat`).
    pub matched: u32,
    /// SubmitCommands that did not find it (`FlGMis`).
    pub mismatched: u32,
    /// Fences completed at SubmitCommand without ever queueing (`FlGImm`).
    pub immediate: u32,
    /// Fences delivered (`FlGRet`).
    pub retired: u32,
    /// Fences delivered after the `WddmHeadMs` rebase (`FlGReb`).
    pub rebased: u32,
    /// Sum of the submit-to-retire lags of the QUEUED fences, microseconds (`FlGLagSum`).
    pub lag_sum_us: u64,
    /// ICD submissions of another Venus context that reached the transport while a stream
    /// gate was open (`FlGUnord`).
    pub overlap: u32,
}

/// What a run's counters say about why the gate does not order a key release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Verdict {
    /// No `HEFL` record reached the KMD: the UMD sent no gate (nothing to explain).
    NoData = 0,
    /// Records arrived, but fewer DMA buffers carrying them reached SubmitCommand than
    /// there were Renders (beyond what batching explains): dxgkrnl dropped or never
    /// submitted the packets, so there was no fence to withhold.
    NotSubmitted = 1,
    /// SubmitCommand did not find the boundary the Render merged: the private data the
    /// Render wrote is not the one SubmitCommand read, and the packet retired by whatever
    /// else it found.
    BoundaryLost = 2,
    /// The fences completed at SubmitCommand without waiting (the boundary was already
    /// satisfiable, or a wait-free arm was taken) in most cases: the gate held nothing.
    RetiredAtSubmit = 3,
    /// The fences retired only through the `WddmHeadMs` rebase (the point never retired).
    Rebased = 4,
    /// Every step is as designed (matched, queued, retired after a real wait) AND work of
    /// another Venus context entered the transport while a gate was open: the KMD side is
    /// honest and the key release is unordered because the acquirer's work does not go
    /// through DMA buffers (`docs/flush-gate.md` section 9).
    ConsumerUnordered = 5,
    /// Every step is as designed and no overlapping work was seen: the KMD side is
    /// honest; the cause is not in what the KMD observes (read the ring).
    GateHonest = 6,
}

/// A fence that waited at least this long (microseconds) is "queued behind real work".
/// A wait-free DPC pickup is a few microseconds; the copies of the failing test take
/// milliseconds.
pub const REAL_WAIT_US: u64 = 100;

/// Reduce a run to the cause its counters support. Argument-only, so the ordering of the
/// checks (the earlier the cause, the more it hides the later ones) is tested.
pub fn verdict(s: &Summary) -> Verdict {
    if s.render == 0 {
        return Verdict::NoData;
    }
    // Renders that were not batched should each have produced a SubmitCommand. A little
    // slack: the last Render may still be waiting for its SubmitCommand when the
    // counters are read.
    let expected = s.render.saturating_sub(s.batched);
    if s.submit.saturating_add(1) < expected {
        return Verdict::NotSubmitted;
    }
    if s.submit == 0 {
        // One Render, not yet submitted: nothing to conclude yet.
        return Verdict::NoData;
    }
    if s.mismatched > s.matched {
        return Verdict::BoundaryLost;
    }
    if s.immediate.saturating_mul(2) > s.submit {
        return Verdict::RetiredAtSubmit;
    }
    if s.rebased > 0 && s.rebased.saturating_mul(2) >= s.retired {
        return Verdict::Rebased;
    }
    let queued_retired = s.retired.saturating_sub(s.immediate);
    let waited = queued_retired != 0
        && s.lag_sum_us / (queued_retired as u64) >= REAL_WAIT_US;
    if waited && s.overlap > 0 {
        return Verdict::ConsumerUnordered;
    }
    Verdict::GateHonest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_records_and_reads_back() {
        let ring = Ring::new();
        assert_eq!(ring.cursor(), 0);
        let a = ring.record(kind::RENDER, flag::STREAM | flag::STAMPED, 0xdead_beef, 0, 41, 0x8000_0001_0000_0029, 1000);
        let b = ring.record(kind::SUBMIT, flag::MATCH, 0xdead_beef, 77, 12, 0x8000_0001_0000_0029, 2000);
        assert_eq!((a, b), (1, 2));
        let e = ring.read(1).unwrap();
        assert_eq!(e.kind, kind::RENDER);
        assert_eq!(e.flags, flag::STREAM | flag::STAMPED);
        assert_eq!(e.ctx_low, 0xdead_beef);
        assert_eq!(e.aux, 41);
        assert_eq!(e.boundary, 0x8000_0001_0000_0029);
        assert_eq!(e.stamp_100ns, 1000);
        let e = ring.read(2).unwrap();
        assert_eq!((e.fence, e.aux, e.stamp_100ns), (77, 12, 2000));
        assert_eq!(ring.read(0), None);
        assert_eq!(ring.read(3), None);
    }

    #[test]
    fn ring_keeps_only_the_last_ring_len_events() {
        let ring = Ring::new();
        for i in 0..(RING_LEN as u32 + 10) {
            ring.record(kind::SUBMIT, 0, 1, i + 1, 0, 0, i as u64);
        }
        let head = ring.cursor();
        assert_eq!(head, RING_LEN as u32 + 10);
        // The oldest ten are gone, the rest read back with their own payload.
        for seq in 1..=10 {
            assert_eq!(ring.read(seq), None, "seq {seq}");
        }
        for seq in 11..=head {
            let e = ring.read(seq).unwrap();
            assert_eq!(e.seq, seq);
            assert_eq!(e.fence, seq);
        }
    }

    #[test]
    fn latest_for_fence_finds_the_newest_of_the_kind() {
        let ring = Ring::new();
        ring.record(kind::SUBMIT, 0, 1, 5, 0, 0, 10);
        ring.record(kind::RENDER, 0, 1, 5, 0, 0, 11);
        ring.record(kind::SUBMIT, 0, 1, 6, 0, 0, 12);
        ring.record(kind::SUBMIT, flag::IMMEDIATE, 1, 5, 0, 0, 13);
        let e = ring.latest_for_fence(kind::SUBMIT, 5).unwrap();
        assert_eq!(e.stamp_100ns, 13);
        assert_eq!(ring.latest_for_fence(kind::SUBMIT, 6).unwrap().stamp_100ns, 12);
        assert_eq!(ring.latest_for_fence(kind::RETIRE, 5), None);
        assert_eq!(ring.latest_for_fence(kind::SUBMIT, 99), None);
    }

    #[test]
    fn latest_for_fence_forgets_what_the_ring_overwrote() {
        let ring = Ring::new();
        ring.record(kind::SUBMIT, 0, 1, 5, 0, 0, 10);
        for i in 0..RING_LEN as u32 {
            ring.record(kind::RENDER, 0, 1, 0, 0, 0, 100 + i as u64);
        }
        assert_eq!(ring.latest_for_fence(kind::SUBMIT, 5), None);
    }

    #[test]
    fn the_sync_knob_is_off_at_zero_and_capped() {
        assert_eq!(clamp_sync_ms(0), 0);
        assert_eq!(clamp_sync_ms(1), 1);
        assert_eq!(clamp_sync_ms(SYNC_MS_MAX), SYNC_MS_MAX);
        assert_eq!(clamp_sync_ms(SYNC_MS_MAX + 1), SYNC_MS_MAX);
        assert_eq!(clamp_sync_ms(u32::MAX), SYNC_MS_MAX);
    }

    #[test]
    fn lag_is_microseconds_and_saturates() {
        assert_eq!(lag_us(0, 10), 1);
        assert_eq!(lag_us(5, 5), 0);
        // The clock stepped back: 0, not a wrapped 71-minute lag.
        assert_eq!(lag_us(100, 50), 0);
        assert_eq!(lag_us(0, 10_000_000), 1_000_000);
        assert_eq!(lag_us(0, u64::MAX), u32::MAX);
    }

    const H: u64 = 1 << 63;
    fn b(handle: u32, value: u32) -> u64 {
        H | (handle as u64) << 32 | value as u64
    }

    #[test]
    fn a_boundary_match_follows_boundary_kept() {
        // Same handle, equal or larger value.
        assert!(submit_matches(b(3, 9), 0, b(3, 9), 0));
        assert!(submit_matches(b(3, 9), 0, b(3, 12), 0));
        // Smaller value, another handle, no record: the packet does not carry this wait.
        assert!(!submit_matches(b(3, 9), 0, b(3, 8), 0));
        assert!(!submit_matches(b(3, 9), 0, b(4, 9), 0));
        assert!(!submit_matches(b(3, 9), 0, 0, 0));
        // "Already complete" waits for nothing: whatever the buffer holds satisfies it.
        assert!(submit_matches(b(3, 0), 0, 0, 0));
        // The wire floor: the merge keeps the larger id.
        assert!(submit_matches(0, 100, 0, 100));
        assert!(submit_matches(0, 100, 0, 250));
        assert!(!submit_matches(0, 100, 0, 99));
        assert!(!submit_matches(0, 100, 0, 0));
        // Nothing expected.
        assert!(submit_matches(0, 0, 0, 0));
    }

    fn healthy() -> Summary {
        Summary {
            render: 40,
            submit: 40,
            batched: 0,
            matched: 40,
            mismatched: 0,
            immediate: 0,
            retired: 40,
            rebased: 0,
            lag_sum_us: 40 * 5_000,
            overlap: 0,
        }
    }

    #[test]
    fn verdict_names_the_first_broken_step() {
        assert_eq!(verdict(&Summary::default()), Verdict::NoData);
        assert_eq!(verdict(&healthy()), Verdict::GateHonest);
        // Honest gate and another context's work entered while it was open.
        assert_eq!(
            verdict(&Summary { overlap: 20, ..healthy() }),
            Verdict::ConsumerUnordered
        );
        // Overlap with no real wait is not the consumer story: the gate held nothing.
        assert_eq!(
            verdict(&Summary { overlap: 20, lag_sum_us: 40 * 5, ..healthy() }),
            Verdict::GateHonest
        );
        // Packets that never reached SubmitCommand.
        assert_eq!(
            verdict(&Summary { submit: 10, matched: 10, retired: 10, ..healthy() }),
            Verdict::NotSubmitted
        );
        // ... unless batching explains the difference.
        assert_ne!(
            verdict(&Summary { submit: 10, matched: 10, retired: 10, batched: 30, ..healthy() }),
            Verdict::NotSubmitted
        );
        // One Render still in flight when read is not a drop.
        assert_ne!(
            verdict(&Summary { submit: 39, matched: 39, retired: 39, ..healthy() }),
            Verdict::NotSubmitted
        );
        // The record SubmitCommand read is not the one the Render wrote.
        assert_eq!(
            verdict(&Summary { matched: 5, mismatched: 35, ..healthy() }),
            Verdict::BoundaryLost
        );
        // Completed at SubmitCommand.
        assert_eq!(
            verdict(&Summary { immediate: 40, lag_sum_us: 0, ..healthy() }),
            Verdict::RetiredAtSubmit
        );
        // Only the rebase ever released them.
        assert_eq!(
            verdict(&Summary { rebased: 40, ..healthy() }),
            Verdict::Rebased
        );
    }

    #[test]
    fn verdict_never_divides_by_zero() {
        // Everything immediate: no queued fence to average over.
        let s = Summary { immediate: 3, submit: 3, render: 3, matched: 3, retired: 3, ..Summary::default() };
        let _ = verdict(&s);
        let s = Summary { render: 1, submit: 1, matched: 1, ..Summary::default() };
        assert_eq!(verdict(&s), Verdict::GateHonest);
    }
}
