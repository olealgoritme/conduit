//! The frame-done table (`FlipLedger`, `docs/dwm-on-nvk.md` 4.5): for each presented buffer, the
//! newest frame in it that the GPU finished, and whether the GPU may be writing it now, as the
//! KMD learns it from the RM fences the presents' WDDM markers carry. Published in the
//! read-ledger page (`helios_protocol::HeliosFrameDoneTable`), where a DWM on NVK reads it at
//! every bind of an app's flip-model buffer and composes the newest finished frame, never
//! waiting.
//!
//! The KMD attaches each fence to the RM gate of the presenting WDDM context when the present's
//! marker is rendered (before dxgkrnl can hand the frame to DWM) and fires it from the
//! `EventReady` DPC. Three rules make the table fresh:
//!
//! * **Frames, not buffers, are pending.** An app runs up to a few frames ahead of the GPU, so a
//!   buffer usually has its next frame queued before the previous one finished. Each frame is a
//!   pending record of its own; the buffer's `completed` is the newest of its frames that
//!   finished, whatever is queued behind it (405.124 kept one fence per buffer and a newer frame
//!   replaced it before it fired: four of five frames were never marked done).
//! * **A context's frames finish in order.** When a fence fires, every earlier pending frame of
//!   the same gate (one WDDM context, one GPU queue) is done too, whether or not its own
//!   `EventReady` came yet.
//! * **Only the oldest pending frame of a gate can be running.** A queue runs its frames in
//!   order, so the buffer of that frame may be written right now (`writing`); the buffer of a
//!   later queued frame still holds its finished content until the frames before it are done.
//!
//! Pure and host-tested; `kmd_render/src/virtio/gpu/rm_gates.rs` drives it under `virtio_lock`
//! and copies changed slots into the page (`adapter/read_ledger.rs`).

/// Buffers tracked at once (`helios_protocol::HELIOS_FRAME_DONE_SLOTS`).
pub const SLOTS: usize = 48;
/// Frames pending at once, all buffers together.
pub const PENDING: usize = 64;

/// One tracked buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// KMD resource id; 0 = free.
    pub resid: u32,
    /// The presenting process ([`owner_of`]).
    pub owner: u32,
    /// Sequence value of the newest frame submitted into the buffer.
    pub submitted: u64,
    /// Sequence value of the newest frame in the buffer the GPU finished (0 = none yet).
    pub completed: u64,
    /// The GPU may be writing the buffer now: its pending frame is the oldest of its context.
    pub writing: bool,
}

impl Entry {
    /// No frame of the buffer pending.
    pub const fn idle(&self) -> bool {
        self.completed >= self.submitted
    }
}

/// One frame submitted and not finished.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Pending {
    live: bool,
    seq: u64,
    gate: u8,
    slot: u8,
    owner: u32,
    fence: u32,
}

/// The table.
#[derive(Clone, Debug)]
pub struct Table {
    pub entries: [Entry; SLOTS],
    pending: [Pending; PENDING],
    /// Live records in `pending`: every operation that finds it 0 returns at once, so fences
    /// of frames nobody tracks (D3D12 `ExecuteCommandLists`, scanout presents) cost one compare.
    live: u32,
    seq: u64,
    /// Frames not tracked (no slot whose buffer had nothing pending), or whose pending record
    /// had to be dropped (the pending list full: the oldest of the context counted done).
    pub overflow: u32,
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

/// A 32-bit name of a kernel process handle (`hKmdProcess`): its two halves folded, so two
/// processes rarely share one. Only ever compared for equality (a swap chain's buffers).
pub const fn owner_of(process: usize) -> u32 {
    let p = process as u64;
    ((p >> 32) as u32) ^ (p as u32) ^ ((p >> 12) as u32)
}

/// Slots whose published copy must be rewritten: a bit per slot.
pub type Changed = u64;
const _: () = assert!(SLOTS <= 64);

impl Table {
    pub const fn new() -> Self {
        Self {
            entries: [Entry { resid: 0, owner: 0, submitted: 0, completed: 0, writing: false }; SLOTS],
            pending: [Pending { live: false, seq: 0, gate: 0, slot: 0, owner: 0, fence: 0 }; PENDING],
            live: 0,
            seq: 0,
            overflow: 0,
        }
    }

    /// A frame into buffer `resid` of `owner` was submitted on gate `gate` with RM fence
    /// `fence`; `fired`: the fence had fired already at the attach (then every earlier frame of
    /// the gate is done as well). Returns the slots that changed (0: not tracked, counted).
    pub fn submit(&mut self, resid: u32, owner: u32, gate: u8, fence: u32, fired: bool) -> Changed {
        if resid == 0 {
            return 0;
        }
        let Some(slot) = self.slot_for(resid) else {
            self.overflow = self.overflow.saturating_add(1);
            return 0;
        };
        self.seq += 1;
        let seq = self.seq;
        let e = &mut self.entries[slot];
        if e.resid != resid {
            *e = Entry { resid, owner, ..Entry::default() };
        }
        e.owner = owner;
        e.submitted = seq;
        let mut changed: Changed = 1 << slot;
        if fired || fence == 0 {
            changed |= self.finish_through(gate, owner, seq);
            let e = &mut self.entries[slot];
            if e.completed < seq {
                e.completed = seq;
            }
        } else {
            let free = match self.pending.iter().position(|p| !p.live) {
                Some(i) => i,
                None => {
                    // Nothing free: the oldest pending frame of the whole table counts done
                    // (frames this old are long finished in any healthy run).
                    self.overflow = self.overflow.saturating_add(1);
                    let mut i = 0;
                    for (j, p) in self.pending.iter().enumerate() {
                        if p.seq < self.pending[i].seq {
                            i = j;
                        }
                    }
                    let p = self.pending[i];
                    changed |= self.finish_through(p.gate, p.owner, p.seq);
                    i
                }
            };
            self.pending[free] = Pending { live: true, seq, gate, slot: slot as u8, owner, fence };
            self.live += 1;
        }
        changed | self.recompute_writing()
    }

    /// The buffer's own slot, else a free one, else the oldest buffer with no pending frame.
    fn slot_for(&self, resid: u32) -> Option<usize> {
        if let Some(i) = self.entries.iter().position(|e| e.resid == resid) {
            return Some(i);
        }
        if let Some(i) = self.entries.iter().position(|e| e.resid == 0) {
            return Some(i);
        }
        (0..SLOTS)
            .filter(|&i| !self.pending.iter().any(|p| p.live && p.slot as usize == i))
            .min_by_key(|&i| self.entries[i].submitted)
    }

    /// Every pending frame of `gate` (and `owner`) up to `seq` finished.
    fn finish_through(&mut self, gate: u8, owner: u32, seq: u64) -> Changed {
        let mut changed: Changed = 0;
        if self.live == 0 {
            return 0;
        }
        for p in self.pending.iter_mut() {
            if p.live && p.gate == gate && p.owner == owner && p.seq <= seq {
                p.live = false;
                self.live -= 1;
                let e = &mut self.entries[p.slot as usize];
                if e.completed < p.seq {
                    e.completed = p.seq;
                }
                changed |= 1 << p.slot;
            }
        }
        changed
    }

    /// `writing` = the slot holds the oldest pending frame of some gate.
    fn recompute_writing(&mut self) -> Changed {
        let mut want = [false; SLOTS];
        for p in self.pending.iter().filter(|p| p.live) {
            let oldest = self
                .pending
                .iter()
                .filter(|q| q.live && q.gate == p.gate && q.owner == p.owner)
                .all(|q| q.seq >= p.seq);
            if oldest {
                want[p.slot as usize] = true;
            }
        }
        let mut changed: Changed = 0;
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.writing != want[i] {
                e.writing = want[i];
                changed |= 1 << i;
            }
        }
        changed
    }

    /// RM fence `fence` of `owner` fired (or expired, or was cancelled with its gate): its frame
    /// and every earlier frame of the same gate finished. Returns the slots that changed.
    pub fn fire(&mut self, fence: u32, owner: u32) -> Changed {
        if fence == 0 || self.live == 0 {
            return 0;
        }
        let Some(p) = self.pending.iter().find(|p| p.live && p.fence == fence && p.owner == owner).copied()
        else {
            return 0;
        };
        self.finish_through(p.gate, p.owner, p.seq) | self.recompute_writing()
    }

    /// Gate `gate` of `owner` went (cancelled, purged): its pending frames count finished.
    pub fn finish_gate(&mut self, gate: u8, owner: u32) -> Changed {
        if self.live == 0 {
            return 0;
        }
        self.finish_through(gate, owner, u64::MAX) | self.recompute_writing()
    }

    /// The process `owner` went: its buffers' slots and pending frames are freed.
    pub fn forget_owner(&mut self, owner: u32) -> Changed {
        for p in self.pending.iter_mut() {
            if p.live && p.owner == owner {
                p.live = false;
                self.live -= 1;
            }
        }
        let mut changed: Changed = 0;
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.resid != 0 && e.owner == owner {
                *e = Entry::default();
                changed |= 1 << i;
            }
        }
        changed | self.recompute_writing()
    }

    /// Frames pending now (diagnostics).
    pub fn pending_count(&self) -> usize {
        self.live as usize
    }

    /// Nothing pending: a fence fire or a gate purge has nothing to do here.
    pub const fn quiet(&self) -> bool {
        self.live == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const G: u8 = 3;
    const O: u32 = 77;

    fn slot(t: &Table, resid: u32) -> Entry {
        *t.entries.iter().find(|e| e.resid == resid).unwrap()
    }

    #[test]
    fn a_frame_is_pending_until_its_fence_fires() {
        let mut t = Table::new();
        t.submit(7, O, G, 100, false);
        assert!(!slot(&t, 7).idle());
        assert!(slot(&t, 7).writing, "the only pending frame of its context may be running");
        assert_eq!(t.fire(100, O + 1), 0, "another process's fence of the same number");
        assert_ne!(t.fire(100, O), 0);
        let e = slot(&t, 7);
        assert!(e.idle() && !e.writing);
        assert_eq!(t.fire(100, O), 0, "a repeat changes nothing");
    }

    #[test]
    fn a_later_fence_of_the_context_finishes_every_earlier_frame() {
        let mut t = Table::new();
        t.submit(1, O, G, 10, false);
        t.submit(2, O, G, 11, false);
        t.submit(3, O, G, 12, false);
        t.fire(12, O);
        for r in 1..=3 {
            assert!(slot(&t, r).idle(), "buffer {r}");
        }
        assert_eq!(t.pending_count(), 0);
    }

    #[test]
    fn a_queued_frame_does_not_hide_the_finished_one_before_it() {
        // Three buffers, the app three frames ahead: buffer 1 holds frame 1 (done) and has
        // frame 4 queued behind frames 2 and 3.
        let mut t = Table::new();
        t.submit(1, O, G, 1, false);
        t.submit(2, O, G, 2, false);
        t.submit(3, O, G, 3, false);
        t.fire(1, O);
        t.submit(1, O, G, 4, false);
        let e1 = slot(&t, 1);
        assert_eq!(e1.completed, 1, "frame 1 stays the buffer's finished content");
        assert!(!e1.idle());
        assert!(!e1.writing, "frame 4 waits for frames 2 and 3");
        assert!(slot(&t, 2).writing, "frame 2 is the oldest pending: it may be running");
        assert!(!slot(&t, 3).writing);
        t.fire(2, O);
        assert!(slot(&t, 3).writing && !slot(&t, 2).writing);
        t.fire(3, O);
        assert!(slot(&t, 1).writing, "now frame 4 may run");
    }

    #[test]
    fn contexts_are_independent() {
        let mut t = Table::new();
        t.submit(1, O, 1, 50, false);
        t.submit(2, O, 2, 51, false);
        t.fire(51, O);
        assert!(!slot(&t, 1).idle(), "a fence of another gate says nothing about gate 1");
        assert!(slot(&t, 2).idle());
        assert!(slot(&t, 1).writing);
    }

    #[test]
    fn a_fence_that_fired_before_the_attach_finishes_the_frame_and_those_before() {
        let mut t = Table::new();
        t.submit(1, O, G, 5, false);
        t.submit(2, O, G, 6, true);
        assert!(slot(&t, 1).idle() && slot(&t, 2).idle());
    }

    #[test]
    fn a_full_table_reuses_the_oldest_buffer_without_pending_frames() {
        let mut t = Table::new();
        for r in 1..=SLOTS as u32 {
            t.submit(r, O, G, 0, true);
        }
        assert_ne!(t.submit(1000, O, G, 0, true), 0);
        assert!(!t.entries.iter().any(|e| e.resid == 1), "resid 1 was the oldest idle buffer");
        let mut busy = Table::new();
        for r in 1..=SLOTS as u32 {
            busy.submit(r, O, (r % 200) as u8, r, false);
        }
        assert_eq!(busy.submit(5000, O, G, 5000, false), 0);
        assert_eq!(busy.overflow, 1);
    }

    #[test]
    fn a_full_pending_list_counts_its_oldest_frame_done() {
        let mut t = Table::new();
        for i in 0..PENDING as u32 {
            t.submit(1 + (i % 3), O, G, 100 + i, false);
        }
        assert_eq!(t.pending_count(), PENDING);
        t.submit(1, O, G, 999, false);
        assert_eq!(t.overflow, 1);
        assert_eq!(t.pending_count(), PENDING);
    }

    #[test]
    fn a_cancelled_gate_and_a_dead_process_leave_nothing_pending() {
        let mut t = Table::new();
        t.submit(1, O, G, 5, false);
        t.submit(2, O + 1, 9, 6, false);
        t.finish_gate(G, O);
        assert!(slot(&t, 1).idle() && !slot(&t, 1).writing);
        assert_ne!(t.forget_owner(O + 1), 0);
        assert!(t.entries.iter().all(|e| e.resid != 2));
        assert_eq!(t.pending_count(), 0);
        assert_eq!(t.submit(0, O, G, 7, false), 0);
    }

    #[test]
    fn the_live_count_follows_the_records_and_a_quiet_table_ignores_fences() {
        let mut t = Table::new();
        assert!(t.quiet());
        assert_eq!(t.fire(5, O), 0);
        t.submit(1, O, G, 5, false);
        t.submit(2, O, G, 6, false);
        assert_eq!(t.pending_count(), 2);
        t.fire(5, O);
        assert_eq!(t.pending_count(), 1);
        t.fire(6, O);
        assert!(t.quiet());
        t.submit(3, O, G, 7, false);
        t.forget_owner(O);
        assert!(t.quiet());
    }

    #[test]
    fn owners_fold_both_halves() {
        assert_ne!(owner_of(0xFFFF_8000_1234_5000), owner_of(0xFFFF_8000_1234_6000));
        assert_eq!(owner_of(0x1000), owner_of(0x1000));
    }
}
