//! The pure rules behind presents that retire on an RM fence
//! (`guest/windows/docs/rm-fence-marker.md`).
//!
//! Three pieces, all fixed-size and allocation-free (they run under the KMD's
//! `virtio_lock` at `DISPATCH_LEVEL`):
//!
//! * [`FenceMeta`], the per-handle state of a fence the KMD tracks: who created it
//!   (a process, for the WDDM carriers' ownership check), whether and how it fired,
//!   what it was attached to, and whether the KMD owes the host a `Close`;
//! * [`ScanoutQueue`], carrier (a): the FIFO of `SCANOUT_PRESENT`s waiting for
//!   their fences, with ready-prefix coalescing;
//! * [`Gate`], carrier (b): the points of one process's "RM gate", the object a
//!   WDDM boundary in the tagged stream namespace names. A point is ready when it
//!   and every earlier point fired.

use crate::foreign_scanout::Flip;

/// What a fence was attached to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// Not attached: the creating caller still owns the handle.
    None,
    /// Carrier (a): an entry of the [`ScanoutQueue`] waits for it.
    Scanout,
    /// Carrier (b): a point of the gate with this index waits for it.
    Gate(u8),
    /// Carrier (b), a `HERF` / `HEPR` tail whose marker could not be attached (both
    /// markers, no gate room, ...): the KMD took the handle anyway and only closes
    /// it. Nothing waits for it.
    Discard,
}

/// Why [`FenceMeta::attach`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachError {
    /// Already attached (to anything): the KMD owns it, or is about to.
    AlreadyAttached,
}

/// Per-fence-handle state. `process` is the opaque `hKmdProcess` of the device that
/// created the fence (0 = unknown: never matches a WDDM carrier).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FenceMeta {
    process: usize,
    fired: Option<i32>,
    attach: Attach,
    close_wanted: bool,
}

impl FenceMeta {
    pub const fn new(process: usize) -> Self {
        Self {
            process,
            fired: None,
            attach: Attach::None,
            close_wanted: false,
        }
    }

    pub const fn process(&self) -> usize {
        self.process
    }

    /// The fence's status once it fired (0 or the fence's error), else `None`.
    pub const fn fired(&self) -> Option<i32> {
        self.fired
    }

    pub const fn attached(&self) -> Attach {
        self.attach
    }

    pub const fn close_wanted(&self) -> bool {
        self.close_wanted
    }

    /// `EventReady` arrived. `true` only for the FIRST one: a fence fires once, so a
    /// repeated notification (or one for a number the host reused) changes nothing.
    pub fn set_fired(&mut self, status: i32) -> bool {
        if self.fired.is_some() {
            return false;
        }
        self.fired = Some(status);
        true
    }

    /// Attach to a carrier. Returns the status it had already fired with, if it
    /// had (an early fire): the carrier treats that present as already complete.
    pub fn attach(&mut self, to: Attach) -> Result<Option<i32>, AttachError> {
        if self.attach != Attach::None || self.close_wanted {
            return Err(AttachError::AlreadyAttached);
        }
        self.attach = to;
        Ok(self.fired)
    }

    /// Undo an attach whose carrier then failed before queueing anything.
    pub fn unattach(&mut self) {
        self.attach = Attach::None;
    }

    /// The KMD must close this handle on the host (it fired, or its present was
    /// dropped). Idempotent. `true` the first time.
    pub fn want_close(&mut self) -> bool {
        !core::mem::replace(&mut self.close_wanted, true)
    }

    /// The host did not take the `Close`: it is owed again at the next sweep.
    pub fn close_failed(&mut self) {
        self.close_wanted = false;
    }
}

/// Whether a WDDM carrier of a present created in `present_process` may attach a
/// fence created in `fence_process`: the same nonzero process.
pub const fn same_process(fence_process: usize, present_process: usize) -> bool {
    fence_process != 0 && fence_process == present_process
}

// ---------------------------------------------------------------------------
// Carrier (a): the SCANOUT_PRESENT queue.
// ---------------------------------------------------------------------------

/// Most presents waiting at once (`HELIOS_NVRM_SCANOUT_FENCE_DEPTH`).
pub const SCANOUT_QUEUE_DEPTH: usize = 8;

/// One queued flip. `fence == 0` means "ready now" (an unfenced present queued
/// behind fenced ones so the order is kept).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QEntry {
    pub flip: Flip,
    pub gem: u32,
    pub fence: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Full;

/// What [`ScanoutQueue::drain`] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Drain {
    /// The flip to send now: the newest of the leading ready entries.
    pub send: Option<QEntry>,
    /// Fence handles the KMD must now close (sent, skipped and dropped ones).
    pub close: [u32; SCANOUT_QUEUE_DEPTH],
    pub nclose: usize,
    /// Ready entries superseded by a newer ready one (never shown).
    pub skipped: u32,
    /// Entries of a source that ended (or of an earlier transport generation).
    pub dropped: u32,
    /// The `seq` of every entry counted in `skipped` and `dropped` (flips that will
    /// never reach the host): the release book retires them. At most the queue's depth
    /// in all.
    pub gone: [u64; SCANOUT_QUEUE_DEPTH],
    pub ngone: usize,
}

impl Drain {
    const fn new() -> Self {
        Self {
            send: None,
            close: [0; SCANOUT_QUEUE_DEPTH],
            nclose: 0,
            skipped: 0,
            dropped: 0,
            gone: [0; SCANOUT_QUEUE_DEPTH],
            ngone: 0,
        }
    }

    fn note_gone(&mut self, seq: u64) {
        if let Some(slot) = self.gone.get_mut(self.ngone) {
            *slot = seq;
            self.ngone += 1;
        }
    }

    /// The seqs of the flips this drain discarded unsent.
    pub fn gone_seqs(&self) -> &[u64] {
        &self.gone[..self.ngone]
    }

    fn close_fence(&mut self, fence: u32) {
        if fence != 0 {
            if let Some(slot) = self.close.get_mut(self.nclose) {
                *slot = fence;
                self.nclose += 1;
            }
        }
    }

    pub fn closes(&self) -> &[u32] {
        &self.close[..self.nclose]
    }
}

/// FIFO of fenced presents. Flips go out in submission order; of the entries at the
/// head that are ready only the newest is sent; an entry that is not ready is
/// never dropped or replaced (only its source ending drops it).
pub struct ScanoutQueue {
    len: usize,
    e: [Option<QEntry>; SCANOUT_QUEUE_DEPTH],
}

impl Default for ScanoutQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ScanoutQueue {
    pub const fn new() -> Self {
        Self {
            len: 0,
            e: [None; SCANOUT_QUEUE_DEPTH],
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn is_full(&self) -> bool {
        self.len >= SCANOUT_QUEUE_DEPTH
    }

    pub fn push(&mut self, entry: QEntry) -> Result<(), Full> {
        if self.is_full() {
            return Err(Full);
        }
        self.e[self.len] = Some(entry);
        self.len += 1;
        Ok(())
    }

    /// Forget everything (transport reset): the fences die with the transport.
    /// Returns how many entries were dropped.
    pub fn clear(&mut self) -> u32 {
        let n = self.len as u32;
        self.len = 0;
        self.e = [None; SCANOUT_QUEUE_DEPTH];
        n
    }

    /// Decide what to do now. `live` is the live source's `(generation, epoch)`
    /// (`None`: no source); entries of any other source or epoch are dropped.
    /// `fired(fence)` says whether a nonzero fence handle has fired.
    pub fn drain(&mut self, live: Option<(u32, u64)>, fired: impl Fn(u32) -> bool) -> Drain {
        let mut out = Drain::new();
        // 1. Drop what can never be shown, keeping the order of the rest.
        let mut keep = 0usize;
        for i in 0..self.len {
            let Some(entry) = self.e[i] else {
                continue;
            };
            if live == Some((entry.flip.generation, entry.flip.epoch)) {
                self.e[keep] = Some(entry);
                keep += 1;
            } else {
                out.dropped += 1;
                out.note_gone(entry.flip.seq);
                out.close_fence(entry.fence);
            }
        }
        for slot in &mut self.e[keep..self.len] {
            *slot = None;
        }
        self.len = keep;
        // 2. The leading run of ready entries: send the newest, skip the others.
        let ready = |e: &QEntry| e.fence == 0 || fired(e.fence);
        let mut run = 0usize;
        while run < self.len && self.e[run].as_ref().is_some_and(ready) {
            run += 1;
        }
        if run == 0 {
            return out;
        }
        for i in 0..run {
            let Some(entry) = self.e[i] else {
                continue;
            };
            out.close_fence(entry.fence);
            if i + 1 == run {
                out.send = Some(entry);
            } else {
                out.skipped += 1;
                out.note_gone(entry.flip.seq);
            }
        }
        // 3. Shift the rest down.
        for i in run..self.len {
            self.e[i - run] = self.e[i];
        }
        for slot in &mut self.e[self.len - run..self.len] {
            *slot = None;
        }
        self.len -= run;
        out
    }
}

// ---------------------------------------------------------------------------
// Carrier (b): the RM gate.
// ---------------------------------------------------------------------------

/// Points one gate can hold unretired. One per `Present` of the process plus one per
/// `ExecuteCommandLists` in flight; 128 is a few frames of a busy D3D12 app.
pub const GATE_POINTS: usize = 128;
/// The first point numbers are 1, 2, ...; a gate refuses past this and the KMD
/// recycles it when idle, so a boundary value never wraps.
pub const GATE_POINT_MAX: u32 = u32::MAX - 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateError {
    /// [`GATE_POINTS`] points are waiting.
    Full,
    /// Point numbers ran out: the gate must be recycled (it can only be once idle).
    Exhausted,
}

#[derive(Clone, Copy)]
struct Point {
    fence: u32,
    fired: bool,
}

/// The points of one gate. Point `p` is stored at `p % GATE_POINTS`; the live window
/// is `(retired, next)`.
pub struct Gate {
    next: u32,
    retired: u32,
    pts: [Point; GATE_POINTS],
}

impl Default for Gate {
    fn default() -> Self {
        Self::new()
    }
}

impl Gate {
    pub const fn new() -> Self {
        Self {
            next: 1,
            retired: 0,
            pts: [Point {
                fence: 0,
                fired: false,
            }; GATE_POINTS],
        }
    }

    /// The highest point `p` such that points `1..=p` all fired.
    pub const fn retired(&self) -> u32 {
        self.retired
    }

    /// Points attached and not yet retired.
    pub const fn pending(&self) -> u32 {
        self.next - 1 - self.retired
    }

    pub const fn is_idle(&self) -> bool {
        self.pending() == 0
    }

    /// Whether the numbers are nearly used up (recycle when idle).
    pub const fn needs_recycle(&self) -> bool {
        self.next > GATE_POINT_MAX
    }

    /// Attach `fence` (nonzero) as the next point; `fired` when it had already fired.
    pub fn attach(&mut self, fence: u32, fired: bool) -> Result<u32, GateError> {
        if self.next > GATE_POINT_MAX {
            return Err(GateError::Exhausted);
        }
        if self.pending() as usize >= GATE_POINTS {
            return Err(GateError::Full);
        }
        let p = self.next;
        self.pts[p as usize % GATE_POINTS] = Point { fence, fired };
        self.next += 1;
        self.advance();
        Ok(p)
    }

    /// `fence` fired. `Some(retired)` if it names a pending point (whether or not
    /// retirement moved), `None` if this gate holds no such unfired point.
    pub fn fire(&mut self, fence: u32) -> Option<u32> {
        if fence == 0 {
            return None;
        }
        let mut p = self.retired + 1;
        while p < self.next {
            let pt = &mut self.pts[p as usize % GATE_POINTS];
            if pt.fence == fence && !pt.fired {
                pt.fired = true;
                self.advance();
                return Some(self.retired);
            }
            p += 1;
        }
        None
    }

    fn advance(&mut self) {
        while self.retired + 1 < self.next
            && self.pts[(self.retired + 1) as usize % GATE_POINTS].fired
        {
            self.retired += 1;
        }
    }

    /// Calls `f` with the fence of every point not yet fired (for a purge: the KMD
    /// closes those handles).
    pub fn for_each_unfired(&self, mut f: impl FnMut(u32)) {
        let mut p = self.retired + 1;
        while p < self.next {
            let pt = self.pts[p as usize % GATE_POINTS];
            if !pt.fired && pt.fence != 0 {
                f(pt.fence);
            }
            p += 1;
        }
    }

    /// Mark the oldest unfired point fired and return its fence (a purge: the caller
    /// closes each handle). `None` once none is left.
    pub fn take_unfired(&mut self) -> Option<u32> {
        let mut p = self.retired + 1;
        while p < self.next {
            let pt = &mut self.pts[p as usize % GATE_POINTS];
            if !pt.fired && pt.fence != 0 {
                pt.fired = true;
                let fence = pt.fence;
                self.advance();
                return Some(fence);
            }
            p += 1;
        }
        None
    }

    /// Back to the empty state (a recycled or purged gate).
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

// ---------------------------------------------------------------------------
// Merging the boundaries of one DMA buffer.
// ---------------------------------------------------------------------------
//
// One DMA buffer can carry several presents (Windows batches them), and the private
// record has room for ONE tagged boundary. Until RM gates existed a context owned at
// most one stream, so two boundaries of a buffer always shared a handle and the
// larger value subsumed the other. A gate is a second handle: a client that mixes
// fenced presents with the CPU-complete marker `(ctx, 0, cookie)`, or a stream with a
// fence, puts two handles in one buffer. Refusing the second Present would fail a
// frame whose fence the KMD already owns, so the rules below never fail a plain
// boundary.

/// What merging a new boundary into a record's boundary produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundaryMerge {
    /// The boundary the record carries from now on.
    pub boundary: u64,
    /// A wait the merge could not keep: the present it named may now be released
    /// before its producer finished (counted by the caller).
    pub dropped: bool,
}

/// Merge `new` (a tagged boundary, validated by the caller) into `old` (what the
/// record carries, 0 for none).
///
/// * nothing before, or the same boundary: `new`;
/// * the same handle: the larger value (values of one handle are monotonic);
/// * `new` has value 0 ("already complete", no dependency): `old` stays, nothing is
///   lost;
/// * `old` has value 0: `new` replaces it, nothing is lost;
/// * two different handles that both wait for something: the OLDER boundary stays
///   and `new` is dropped (`dropped`). A record holds one boundary, so one wait is
///   lost either way. The wire-watermark fallback would lose BOTH (and does not
///   cover an RM gate's work at all), whereas keeping the older one keeps the
///   earliest wait of the buffer and costs only the later present's wait. The
///   present is never failed: its fence was taken at Render.
pub fn merge_stream_boundaries(old: u64, new: u64) -> BoundaryMerge {
    merge_stream_boundaries_with(old, new, false)
}

/// As [`merge_stream_boundaries`], for a record whose boundary carries a
/// WindowedBlt token (`old_pinned`): the token is valid only under that boundary, so
/// a different handle can never replace it, even one with value 0 (only the new
/// wait, if it has one, is lost).
pub fn merge_stream_boundaries_with(old: u64, new: u64, old_pinned: bool) -> BoundaryMerge {
    use crate::present_stream::{decode_boundary, encode_boundary};
    let Some((nh, nv)) = decode_boundary(new) else {
        return BoundaryMerge {
            boundary: old,
            dropped: new != 0,
        };
    };
    let Some((oh, ov)) = decode_boundary(old) else {
        // Nothing usable before (an untagged value is not one this crate writes).
        return BoundaryMerge {
            boundary: new,
            dropped: false,
        };
    };
    if oh == nh {
        return BoundaryMerge {
            boundary: encode_boundary(oh, ov.max(nv)),
            dropped: false,
        };
    }
    if nv == 0 {
        return BoundaryMerge {
            boundary: old,
            dropped: false,
        };
    }
    if ov == 0 && !old_pinned {
        return BoundaryMerge {
            boundary: new,
            dropped: false,
        };
    }
    BoundaryMerge {
        boundary: old,
        dropped: true,
    }
}

/// What merging a WindowedBlt request into a record produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BltMerge {
    pub boundary: u64,
    pub token: u64,
    /// The record's earlier boundary waited for something and was replaced.
    pub dropped: bool,
}

/// Merge a WindowedBlt `(token, boundary)` into the record's `(boundary, token)`.
/// `None`: the request cannot be carried (the caller cancels it and fails the
/// Present, as before): `token == 0`, an unusable `boundary`, or two requests of
/// different handles in one buffer.
///
/// A token is only valid under the boundary it was queued with (`SubmitCommand`
/// promotes it when that boundary is reached), so unlike a plain boundary it cannot
/// stay behind a different one. The same handle merges as before (larger token and
/// value). A different handle replaces an old boundary that carries no token: the
/// request, which exists only under its own boundary, wins, and the replaced wait is
/// `dropped` unless it was value 0.
pub fn merge_blt_boundaries(
    old_boundary: u64,
    old_token: u64,
    boundary: u64,
    token: u64,
) -> Option<BltMerge> {
    use crate::present_stream::{decode_boundary, encode_boundary};
    let (nh, nv) = decode_boundary(boundary)?;
    if token == 0 {
        return None;
    }
    let token_max = old_token.max(token);
    let Some((oh, ov)) = decode_boundary(old_boundary) else {
        return Some(BltMerge {
            boundary,
            token: token_max,
            dropped: false,
        });
    };
    if oh == nh {
        return Some(BltMerge {
            boundary: if old_boundary == boundary {
                boundary
            } else {
                encode_boundary(nh, ov.max(nv))
            },
            token: token_max,
            dropped: false,
        });
    }
    if old_token != 0 {
        return None;
    }
    Some(BltMerge {
        boundary,
        token,
        dropped: ov != 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::foreign_scanout::Layout;

    fn flip(seq: u64, generation: u32, epoch: u64) -> Flip {
        Flip {
            seq,
            generation,
            handle: 5,
            epoch,
            layout: Layout {
                width: 64,
                height: 64,
                stride: 256,
                offset: 0,
                fourcc: crate::foreign_scanout::FOURCC_XRGB8888,
                modifier: 0,
            },
        }
    }

    fn entry(seq: u64, fence: u32) -> QEntry {
        QEntry {
            flip: flip(seq, 1, 7),
            gem: seq as u32 + 100,
            fence,
        }
    }

    // ---- FenceMeta: registered -> fired -> retired ------------------------------

    #[test]
    fn a_fence_fires_once_and_the_first_status_stands() {
        let mut m = FenceMeta::new(9);
        assert_eq!(m.fired(), None);
        assert!(m.set_fired(-62));
        assert!(!m.set_fired(0), "a repeat is not a second fire");
        assert_eq!(
            m.fired(),
            Some(-62),
            "error status is kept, not overwritten"
        );
    }

    #[test]
    fn attach_hands_back_an_early_fire_and_only_attaches_once() {
        let mut m = FenceMeta::new(9);
        assert_eq!(m.attach(Attach::Scanout), Ok(None));
        assert_eq!(m.attach(Attach::Gate(0)), Err(AttachError::AlreadyAttached));
        let mut early = FenceMeta::new(9);
        early.set_fired(0);
        assert_eq!(early.attach(Attach::Gate(2)), Ok(Some(0)));
        assert_eq!(early.attached(), Attach::Gate(2));
    }

    #[test]
    fn a_handle_the_kmd_will_close_cannot_be_attached() {
        let mut m = FenceMeta::new(9);
        assert!(m.want_close());
        assert!(!m.want_close());
        assert_eq!(m.attach(Attach::Scanout), Err(AttachError::AlreadyAttached));
        m.close_failed();
        assert!(!m.close_wanted());
    }

    #[test]
    fn process_check_needs_a_known_process() {
        assert!(same_process(5, 5));
        assert!(!same_process(5, 6));
        assert!(!same_process(0, 0), "an unknown creator never matches");
    }

    // ---- ScanoutQueue --------------------------------------------------------------

    #[test]
    fn fifo_waits_behind_an_unfired_head_even_if_a_later_fence_fired() {
        let mut q = ScanoutQueue::new();
        q.push(entry(1, 10)).unwrap();
        q.push(entry(2, 11)).unwrap();
        let d = q.drain(Some((1, 7)), |f| f == 11);
        assert_eq!(d.send, None);
        assert_eq!(d.nclose, 0);
        assert_eq!(q.len(), 2);
    }

    #[test]
    fn ready_prefix_sends_only_the_newest_and_skips_the_rest() {
        let mut q = ScanoutQueue::new();
        for (s, f) in [(1, 10), (2, 11), (3, 12), (4, 13)] {
            q.push(entry(s, f)).unwrap();
        }
        // 10, 11, 12 fired; 13 did not.
        let d = q.drain(Some((1, 7)), |f| f <= 12);
        assert_eq!(d.send.map(|e| e.flip.seq), Some(3));
        assert_eq!(d.skipped, 2);
        assert_eq!(d.closes(), &[10, 11, 12]);
        // The skipped flips never reach the host: the release book retires them.
        assert_eq!(d.gone_seqs(), &[1, 2]);
        assert_eq!(q.len(), 1);
        // The pending one is untouched and sent when it fires.
        let d = q.drain(Some((1, 7)), |_| true);
        assert_eq!(d.send.map(|e| e.flip.seq), Some(4));
        assert_eq!(d.skipped, 0);
        assert!(q.is_empty());
    }

    #[test]
    fn an_already_fired_fence_on_an_empty_queue_is_sent_at_once() {
        let mut q = ScanoutQueue::new();
        q.push(entry(1, 10)).unwrap();
        let d = q.drain(Some((1, 7)), |_| true);
        assert_eq!(d.send.map(|e| e.gem), Some(101));
        assert_eq!(d.closes(), &[10]);
    }

    #[test]
    fn an_unfenced_entry_keeps_the_order_behind_fenced_ones() {
        let mut q = ScanoutQueue::new();
        q.push(entry(1, 10)).unwrap();
        q.push(entry(2, 0)).unwrap();
        let d = q.drain(Some((1, 7)), |_| false);
        assert_eq!(
            d.send, None,
            "the unfenced frame waits behind the fenced one"
        );
        let d = q.drain(Some((1, 7)), |f| f == 10);
        assert_eq!(d.send.map(|e| e.flip.seq), Some(2));
        assert_eq!(d.skipped, 1);
        assert_eq!(d.closes(), &[10], "fence 0 is never closed");
    }

    #[test]
    fn an_ended_source_or_epoch_drops_its_entries_and_closes_their_fences() {
        let mut q = ScanoutQueue::new();
        q.push(entry(1, 10)).unwrap();
        q.push(entry(2, 11)).unwrap();
        // The source changed generation: nothing is shown, nothing leaks.
        let d = q.drain(Some((2, 7)), |_| true);
        assert_eq!(d.send, None);
        assert_eq!(d.dropped, 2);
        assert_eq!(d.closes(), &[10, 11]);
        assert_eq!(d.gone_seqs(), &[1, 2]);
        assert!(q.is_empty());
        // No source at all, and a stale epoch.
        q.push(entry(3, 12)).unwrap();
        assert_eq!(q.drain(None, |_| true).dropped, 1);
        q.push(entry(4, 13)).unwrap();
        assert_eq!(q.drain(Some((1, 8)), |_| true).dropped, 1);
    }

    #[test]
    fn mixed_live_and_dead_entries_keep_the_live_order() {
        let mut q = ScanoutQueue::new();
        let mut dead = entry(1, 10);
        dead.flip.generation = 9;
        q.push(dead).unwrap();
        q.push(entry(2, 11)).unwrap();
        let d = q.drain(Some((1, 7)), |_| false);
        assert_eq!(d.dropped, 1);
        assert_eq!(d.closes(), &[10]);
        assert_eq!(d.gone_seqs(), &[1]);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn the_queue_is_bounded_and_clear_reports_what_it_held() {
        let mut q = ScanoutQueue::new();
        for i in 0..SCANOUT_QUEUE_DEPTH as u64 {
            q.push(entry(i, 10 + i as u32)).unwrap();
        }
        assert!(q.is_full());
        assert_eq!(q.push(entry(99, 99)), Err(Full));
        assert_eq!(q.clear(), SCANOUT_QUEUE_DEPTH as u32);
        assert!(q.is_empty());
    }

    #[test]
    fn a_whole_queue_dropped_names_every_seq() {
        let mut q = ScanoutQueue::new();
        for i in 1..=SCANOUT_QUEUE_DEPTH as u64 {
            q.push(entry(i, 10 + i as u32)).unwrap();
        }
        let d = q.drain(None, |_| true);
        assert_eq!(d.dropped, SCANOUT_QUEUE_DEPTH as u32);
        assert_eq!(d.ngone, SCANOUT_QUEUE_DEPTH);
        assert_eq!(d.gone_seqs(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    // ---- Gate ------------------------------------------------------------------------

    #[test]
    fn points_are_numbered_from_one_and_retire_in_order() {
        let mut g = Gate::new();
        assert_eq!(g.attach(10, false), Ok(1));
        assert_eq!(g.attach(11, false), Ok(2));
        assert_eq!(g.pending(), 2);
        assert_eq!(g.retired(), 0);
        // Out of order: point 2 fired, point 1 not: nothing retires.
        assert_eq!(g.fire(11), Some(0));
        assert_eq!(g.retired(), 0);
        assert_eq!(g.fire(10), Some(2));
        assert!(g.is_idle());
    }

    #[test]
    fn an_early_fire_retires_at_attach() {
        let mut g = Gate::new();
        assert_eq!(g.attach(10, true), Ok(1));
        assert_eq!(g.retired(), 1);
        // And behind an unfired point it waits.
        assert_eq!(g.attach(11, false), Ok(2));
        assert_eq!(g.attach(12, true), Ok(3));
        assert_eq!(g.retired(), 1);
        assert_eq!(g.fire(11), Some(3));
    }

    #[test]
    fn a_fire_is_applied_once_and_unknown_fences_are_ignored() {
        let mut g = Gate::new();
        g.attach(10, false).unwrap();
        assert_eq!(g.fire(99), None);
        assert_eq!(g.fire(0), None);
        assert_eq!(g.fire(10), Some(1));
        assert_eq!(g.fire(10), None, "double retire is impossible");
    }

    #[test]
    fn the_gate_is_bounded_and_frees_room_as_points_retire() {
        let mut g = Gate::new();
        for i in 0..GATE_POINTS as u32 {
            assert_eq!(g.attach(1000 + i, false), Ok(i + 1));
        }
        assert_eq!(g.attach(1, false), Err(GateError::Full));
        assert_eq!(g.fire(1000), Some(1));
        // The slot of point 1 is reused by point GATE_POINTS + 1.
        assert_eq!(g.attach(5000, false), Ok(GATE_POINTS as u32 + 1));
        // Draining everything in order keeps the prefix rule across the wrap.
        for i in 1..GATE_POINTS as u32 {
            g.fire(1000 + i);
        }
        assert_eq!(g.retired(), GATE_POINTS as u32);
        assert_eq!(g.fire(5000), Some(GATE_POINTS as u32 + 1));
    }

    #[test]
    fn numbers_run_out_cleanly_and_a_reset_starts_over() {
        let mut g = Gate::new();
        g.next = GATE_POINT_MAX;
        g.retired = GATE_POINT_MAX - 1;
        assert_eq!(g.attach(1, true), Ok(GATE_POINT_MAX));
        assert!(g.needs_recycle());
        assert_eq!(g.attach(2, true), Err(GateError::Exhausted));
        g.reset();
        assert_eq!(g.attach(2, false), Ok(1));
    }

    #[test]
    fn a_purge_takes_each_unfired_fence_once_and_leaves_the_gate_idle() {
        let mut g = Gate::new();
        g.attach(10, false).unwrap();
        g.attach(11, true).unwrap();
        g.attach(12, false).unwrap();
        assert_eq!(g.take_unfired(), Some(10));
        assert_eq!(g.take_unfired(), Some(12));
        assert_eq!(g.take_unfired(), None);
        assert!(g.is_idle());
    }

    #[test]
    fn a_purge_can_list_exactly_the_unfired_fences() {
        let mut g = Gate::new();
        g.attach(10, false).unwrap();
        g.attach(11, true).unwrap();
        g.attach(12, false).unwrap();
        g.fire(10);
        let mut left = [0u32; 4];
        let mut n = 0;
        g.for_each_unfired(|f| {
            left[n] = f;
            n += 1;
        });
        assert_eq!(&left[..n], &[12]);
    }

    // ---- a gate boundary through the existing boundary machinery ------------------

    use crate::execution_completion::{advances_context, Progress, Record, Wait};
    use crate::present_stream::{
        decode_boundary, encode_boundary, slot_handle, slot_ready, GENERATION_MAX, MAX_STREAMS,
    };

    /// The stream slot a gate rides on, as the KMD drives it: `progress` follows the
    /// gate's `retired()`.
    fn sync(progress: &mut Progress, gate: &Gate) {
        progress.advance_to(gate.retired());
    }

    #[test]
    fn a_gate_boundary_round_trips_in_the_stream_namespace_and_not_as_a_wire_fence() {
        let handle = slot_handle(3, 5);
        for point in [1u32, 2, 77, GATE_POINT_MAX] {
            let boundary = encode_boundary(handle, point);
            assert_eq!(decode_boundary(boundary), Some((handle, point)));
            // Tagged: it is never read as a legacy exclusive wire fence.
            assert_eq!(boundary >> 63, 1);
        }
        // Every wire fence of the legacy namespace (tag clear) is not a boundary.
        for wire in [0u64, 1, 77, (1u64 << 63) - 1] {
            assert_eq!(decode_boundary(wire), None);
        }
        // The largest handle the table can mint stays inside the reserved 31 bits.
        assert!(slot_handle(GENERATION_MAX, MAX_STREAMS - 1) < (1 << 31));
    }

    #[test]
    fn registered_fired_retired_for_a_present_that_waits_on_its_fence() {
        let handle = slot_handle(1, 0);
        let mut gate = Gate::new();
        let mut progress = Progress::EMPTY;
        // Registered: the present names point 1; not ready.
        let p = gate.attach(10, false).unwrap();
        let boundary = encode_boundary(handle, p);
        let (h, value) = decode_boundary(boundary).unwrap();
        assert!(!slot_ready(true, 1, 0, h, value, progress.retired()));
        // Fired: ready, and the execution wait observes it.
        let mut wait = Wait::new(boundary).unwrap();
        wait.observe(h, progress.completed());
        assert!(!wait.completed());
        gate.fire(10);
        sync(&mut progress, &gate);
        assert!(slot_ready(true, 1, 0, h, value, progress.retired()));
        wait.observe(h, progress.completed());
        assert!(wait.completed());
    }

    #[test]
    fn an_error_status_retires_the_present_like_success() {
        // The gate records no status: a fence is "no longer pending" either way. The
        // status is counted by the caller from `FenceMeta`.
        let mut meta = FenceMeta::new(1);
        let mut gate = Gate::new();
        let p = gate.attach(10, false).unwrap();
        assert!(meta.set_fired(-62));
        gate.fire(10);
        assert_eq!(gate.retired(), p);
        assert_eq!(meta.fired(), Some(-62));
    }

    #[test]
    fn an_early_fire_is_ready_at_once_for_the_boundary_it_is_given() {
        let handle = slot_handle(1, 0);
        let mut gate = Gate::new();
        let mut progress = Progress::EMPTY;
        let mut meta = FenceMeta::new(1);
        meta.set_fired(0);
        let early = meta.attach(Attach::Gate(0)).unwrap();
        let p = gate.attach(10, early.is_some()).unwrap();
        sync(&mut progress, &gate);
        assert!(slot_ready(true, 1, 0, handle, p, progress.retired()));
    }

    #[test]
    fn two_presents_of_one_dma_buffer_wait_for_both_fences() {
        let handle = slot_handle(1, 0);
        let mut gate = Gate::new();
        let mut progress = Progress::EMPTY;
        let b1 = encode_boundary(handle, gate.attach(10, false).unwrap());
        let b2 = encode_boundary(handle, gate.attach(11, false).unwrap());
        // The context's stream may only advance, and the record keeps the later one.
        assert!(advances_context(b1, b2));
        let record = Record::default().merge(b1).unwrap().merge(b2).unwrap();
        assert_eq!(record.boundary_for(handle), Some(b2));
        // The later fence firing first retires nothing: the merged boundary (point 2)
        // is still not ready.
        gate.fire(11);
        sync(&mut progress, &gate);
        assert!(!slot_ready(true, 1, 0, handle, 2, progress.retired()));
        gate.fire(10);
        sync(&mut progress, &gate);
        assert!(slot_ready(true, 1, 0, handle, 2, progress.retired()));
    }

    #[test]
    fn a_reset_gate_is_never_success_for_a_boundary_it_issued() {
        let handle = slot_handle(1, 0);
        let mut gate = Gate::new();
        let mut progress = Progress::EMPTY;
        let boundary = encode_boundary(handle, gate.attach(10, false).unwrap());
        let (h, value) = decode_boundary(boundary).unwrap();
        // Teardown: the unfired fences are handed to the closer, the gate empties,
        // and the stream slot dies. A dead slot is not ready (the wait is discharged
        // explicitly, never read as retired).
        assert_eq!(gate.take_unfired(), Some(10));
        gate.reset();
        sync(&mut progress, &gate);
        assert!(!slot_ready(false, 1, 0, h, value, u32::MAX));
        // And a recreated gate (next generation) does not satisfy the old handle.
        assert!(!slot_ready(true, 2, 0, h, value, u32::MAX));
        assert!(Wait::new(boundary).is_some_and(|w| !w.completed()));
    }

    #[test]
    fn progress_follows_the_gate_and_never_regresses() {
        let mut gate = Gate::new();
        let mut progress = Progress::EMPTY;
        gate.attach(10, true).unwrap();
        gate.attach(11, true).unwrap();
        sync(&mut progress, &gate);
        assert_eq!(progress.retired(), 2);
        gate.reset();
        sync(&mut progress, &gate);
        assert_eq!(
            progress.retired(),
            2,
            "a reset gate cannot lower retirement"
        );
    }

    // ---- merging the boundaries of one DMA buffer ---------------------------------

    use crate::present_stream::encode_boundary as enc;

    #[test]
    fn same_handle_takes_the_larger_value_in_either_order() {
        let m = merge_stream_boundaries(enc(5, 3), enc(5, 9));
        assert_eq!((m.boundary, m.dropped), (enc(5, 9), false));
        let m = merge_stream_boundaries(enc(5, 9), enc(5, 3));
        assert_eq!((m.boundary, m.dropped), (enc(5, 9), false));
        let m = merge_stream_boundaries(enc(5, 9), enc(5, 9));
        assert_eq!((m.boundary, m.dropped), (enc(5, 9), false));
    }

    #[test]
    fn an_empty_record_takes_the_boundary_whatever_it_is() {
        for b in [enc(5, 3), enc(5, 0), enc(0x7fff, u32::MAX)] {
            let m = merge_stream_boundaries(0, b);
            assert_eq!((m.boundary, m.dropped), (b, false));
        }
    }

    #[test]
    fn a_cpu_complete_marker_of_another_handle_is_a_no_op_beside_a_wait() {
        // The reported case: a fenced present, then a CPU-complete marker of a
        // registered stream, in one DMA buffer. The second must not fail.
        let gate = enc(0x41, 7);
        let stream_complete = enc(0x82, 0);
        let m = merge_stream_boundaries(gate, stream_complete);
        assert_eq!((m.boundary, m.dropped), (gate, false));
        // And the other order: the complete marker first, the wait replaces it.
        let m = merge_stream_boundaries(stream_complete, gate);
        assert_eq!((m.boundary, m.dropped), (gate, false));
    }

    #[test]
    fn two_waits_of_different_handles_keep_the_older_and_say_so() {
        let gate = enc(0x41, 7);
        let stream = enc(0x82, 3);
        let m = merge_stream_boundaries(gate, stream);
        assert_eq!((m.boundary, m.dropped), (gate, true));
        let m = merge_stream_boundaries(stream, gate);
        assert_eq!((m.boundary, m.dropped), (stream, true));
    }

    #[test]
    fn two_complete_markers_of_different_handles_keep_one_without_loss() {
        let m = merge_stream_boundaries(enc(0x41, 0), enc(0x82, 0));
        assert_eq!((m.boundary, m.dropped), (enc(0x41, 0), false));
    }

    #[test]
    fn a_boundary_that_carries_a_blt_token_is_never_replaced_by_another_handle() {
        let pinned = enc(0x82, 0);
        let m = merge_stream_boundaries_with(pinned, enc(0x41, 7), true);
        assert_eq!((m.boundary, m.dropped), (pinned, true));
        let m = merge_stream_boundaries_with(pinned, enc(0x41, 0), true);
        assert_eq!((m.boundary, m.dropped), (pinned, false));
        // The same handle still merges, token or not.
        let m = merge_stream_boundaries_with(enc(0x82, 3), enc(0x82, 5), true);
        assert_eq!((m.boundary, m.dropped), (enc(0x82, 5), false));
    }

    #[test]
    fn an_unusable_new_boundary_never_replaces_the_record() {
        let m = merge_stream_boundaries(enc(5, 3), 12345);
        assert_eq!((m.boundary, m.dropped), (enc(5, 3), true));
        let m = merge_stream_boundaries(enc(5, 3), 0);
        assert_eq!((m.boundary, m.dropped), (enc(5, 3), false));
    }

    #[test]
    fn a_merge_never_invents_a_dependency_or_lowers_one() {
        // For every pair the result is one of the inputs or the same-handle max, and
        // never below the larger value of a same-handle pair; a wait is reported
        // dropped only when both really wait.
        let hs = [1u32, 2, 3];
        let vs = [0u32, 1, 5, u32::MAX];
        for &ha in &hs {
            for &va in &vs {
                for &hb in &hs {
                    for &vb in &vs {
                        let (a, b) = (enc(ha, va), enc(hb, vb));
                        let m = merge_stream_boundaries(a, b);
                        if ha == hb {
                            assert_eq!(m.boundary, enc(ha, va.max(vb)));
                            assert!(!m.dropped);
                        } else {
                            assert!(m.boundary == a || m.boundary == b);
                            assert_eq!(m.dropped, va != 0 && vb != 0);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_blt_token_merges_under_one_handle_as_it_always_did() {
        let m = merge_blt_boundaries(0, 0, enc(5, 3), 10).unwrap();
        assert_eq!((m.boundary, m.token, m.dropped), (enc(5, 3), 10, false));
        let m = merge_blt_boundaries(enc(5, 3), 10, enc(5, 9), 11).unwrap();
        assert_eq!((m.boundary, m.token, m.dropped), (enc(5, 9), 11, false));
        let m = merge_blt_boundaries(enc(5, 9), 11, enc(5, 3), 12).unwrap();
        assert_eq!((m.boundary, m.token, m.dropped), (enc(5, 9), 12, false));
        // A plain boundary of the same handle before the first token.
        let m = merge_blt_boundaries(enc(5, 4), 0, enc(5, 2), 7).unwrap();
        assert_eq!((m.boundary, m.token, m.dropped), (enc(5, 4), 7, false));
    }

    #[test]
    fn a_blt_token_replaces_a_plain_boundary_of_another_handle() {
        // The token lives only under its own boundary; the replaced wait is counted
        // unless it was value 0.
        let m = merge_blt_boundaries(enc(0x82, 3), 0, enc(0x41, 7), 9).unwrap();
        assert_eq!((m.boundary, m.token, m.dropped), (enc(0x41, 7), 9, true));
        let m = merge_blt_boundaries(enc(0x82, 0), 0, enc(0x41, 7), 9).unwrap();
        assert_eq!((m.boundary, m.token, m.dropped), (enc(0x41, 7), 9, false));
    }

    #[test]
    fn two_blt_requests_of_different_handles_cannot_share_a_buffer() {
        assert_eq!(merge_blt_boundaries(enc(0x82, 3), 4, enc(0x41, 7), 9), None);
    }

    #[test]
    fn a_blt_merge_refuses_what_the_caller_should_not_have_sent() {
        assert_eq!(merge_blt_boundaries(0, 0, enc(5, 3), 0), None);
        assert_eq!(merge_blt_boundaries(0, 0, 77, 1), None);
        assert_eq!(merge_blt_boundaries(0, 0, 0, 1), None);
    }

    #[test]
    fn a_discarded_fence_is_attached_to_nothing_but_not_attachable_again() {
        let mut m = FenceMeta::new(9);
        assert_eq!(m.attach(Attach::Discard), Ok(None));
        assert_eq!(m.attached(), Attach::Discard);
        assert_eq!(m.attach(Attach::Scanout), Err(AttachError::AlreadyAttached));
        assert!(m.want_close());
    }
}
