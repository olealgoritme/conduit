//! The frame-done table (`FlipLedger`, `docs/dwm-on-nvk.md` 4.5): for each presented buffer,
//! whether a frame is still in flight on the GPU, as the KMD learns it from the RM fence the
//! present's WDDM marker carries. Published in the read-ledger page
//! (`helios_protocol::HeliosFrameDoneTable`), where a DWM on NVK reads it at every bind of an
//! app's flip-model buffer and composes the newest buffer whose frame is done, never waiting.
//!
//! Why the KMD: it is the first to know. It attaches the fence to an RM gate when the
//! present's marker is rendered (before dxgkrnl can hand the frame to DWM), and the fence's
//! `EventReady` fires the gate from the DPC. A completion published by the app's own CPU thread
//! (the hand-off ledger's fence worker) lagged the GPU by about a frame at 400 fps, so all
//! three buffers of a chain looked busy and DWM waited.
//!
//! Pure and host-tested; `kmd_render/src/virtio/gpu/rm_gates.rs` drives it under `virtio_lock`
//! and copies changed entries into the page (`adapter/read_ledger.rs`).

/// Buffers tracked at once (`helios_protocol::HELIOS_FRAME_DONE_SLOTS`).
pub const SLOTS: usize = 64;

/// One tracked buffer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Entry {
    /// KMD resource id; 0 = free.
    pub resid: u32,
    /// The presenting process ([`owner_of`]).
    pub owner: u32,
    /// Sequence value of the newest frame submitted.
    pub submitted: u64,
    /// Sequence value of the newest frame done (`>= submitted`: nothing in flight).
    pub completed: u64,
    /// The RM fence of the newest frame, until it fires (0 = none pending).
    pub pending_fence: u32,
}

impl Entry {
    /// No frame in flight.
    pub const fn idle(&self) -> bool {
        self.completed >= self.submitted
    }
}

/// The table.
#[derive(Clone, Debug)]
pub struct Table {
    pub entries: [Entry; SLOTS],
    seq: u64,
    /// Frames not tracked: every slot had a frame in flight.
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

impl Table {
    pub const fn new() -> Self {
        Self {
            entries: [Entry { resid: 0, owner: 0, submitted: 0, completed: 0, pending_fence: 0 }; SLOTS],
            seq: 0,
            overflow: 0,
        }
    }

    /// A frame of buffer `resid` of `owner` was submitted with RM fence `fence`; `fired`: the
    /// fence had fired already at the attach. Returns the slot that changed, or `None` (no
    /// resource id, or no slot: every tracked buffer has a frame in flight; counted).
    ///
    /// The slot is the buffer's own, else a free one, else the idle one whose newest frame is
    /// the oldest. A newer frame of the same buffer replaces the pending fence: the older
    /// fence's fire then changes nothing, and the buffer becomes idle with the newer one.
    pub fn submit(&mut self, resid: u32, owner: u32, fence: u32, fired: bool) -> Option<usize> {
        if resid == 0 {
            return None;
        }
        let i = self.slot_for(resid)?;
        self.seq += 1;
        let e = &mut self.entries[i];
        if e.resid != resid {
            *e = Entry { resid, owner, ..Entry::default() };
        }
        e.owner = owner;
        e.submitted = self.seq;
        if fired || fence == 0 {
            e.completed = self.seq;
            e.pending_fence = 0;
        } else {
            e.pending_fence = fence;
        }
        Some(i)
    }

    fn slot_for(&mut self, resid: u32) -> Option<usize> {
        if let Some(i) = self.entries.iter().position(|e| e.resid == resid) {
            return Some(i);
        }
        if let Some(i) = self.entries.iter().position(|e| e.resid == 0) {
            return Some(i);
        }
        let lru = self
            .entries
            .iter()
            .enumerate()
            .filter(|(_, e)| e.idle())
            .min_by_key(|(_, e)| e.completed)
            .map(|(i, _)| i);
        if lru.is_none() {
            self.overflow = self.overflow.saturating_add(1);
        }
        lru
    }

    /// RM fence `fence` of `owner` fired (or expired, or was cancelled with its gate): the frame
    /// it stood for is done. Returns the slot that changed.
    pub fn fire(&mut self, fence: u32, owner: u32) -> Option<usize> {
        if fence == 0 {
            return None;
        }
        let i = self
            .entries
            .iter()
            .position(|e| e.resid != 0 && e.pending_fence == fence && e.owner == owner)?;
        let e = &mut self.entries[i];
        e.completed = e.submitted;
        e.pending_fence = 0;
        Some(i)
    }

    /// The process `owner` went: its buffers' slots are freed. Returns a bit per changed slot.
    pub fn forget_owner(&mut self, owner: u32) -> u64 {
        let mut changed = 0u64;
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.resid != 0 && e.owner == owner {
                *e = Entry::default();
                changed |= 1 << i;
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_is_in_flight_until_its_fence_fires() {
        let mut t = Table::new();
        let i = t.submit(7, 1, 100, false).unwrap();
        assert!(!t.entries[i].idle());
        assert_eq!(t.fire(100, 2), None, "another process's fence of the same number");
        assert_eq!(t.fire(100, 1), Some(i));
        assert!(t.entries[i].idle());
        assert_eq!(t.fire(100, 1), None, "a repeat changes nothing");
    }

    #[test]
    fn a_fence_that_fired_before_the_attach_leaves_the_buffer_idle() {
        let mut t = Table::new();
        let i = t.submit(7, 1, 100, true).unwrap();
        assert!(t.entries[i].idle());
    }

    #[test]
    fn the_newest_done_buffer_has_the_largest_completed_value() {
        let mut t = Table::new();
        let a = t.submit(1, 9, 10, false).unwrap();
        let b = t.submit(2, 9, 11, false).unwrap();
        let c = t.submit(3, 9, 12, false).unwrap();
        t.fire(10, 9);
        t.fire(11, 9);
        assert!(t.entries[a].idle() && t.entries[b].idle() && !t.entries[c].idle());
        assert!(t.entries[b].completed > t.entries[a].completed);
    }

    #[test]
    fn a_newer_frame_of_a_buffer_supersedes_the_pending_fence() {
        let mut t = Table::new();
        let i = t.submit(5, 1, 20, false).unwrap();
        t.submit(5, 1, 21, false);
        assert_eq!(t.fire(20, 1), None);
        assert!(!t.entries[i].idle());
        assert_eq!(t.fire(21, 1), Some(i));
        assert!(t.entries[i].idle());
    }

    #[test]
    fn a_full_table_reuses_the_oldest_idle_buffer_and_counts_when_none_is_idle() {
        let mut t = Table::new();
        for r in 1..=SLOTS as u32 {
            t.submit(r, 1, 0, true);
        }
        let reused = t.submit(1000, 1, 0, true).unwrap();
        assert_eq!(t.entries[reused].resid, 1000);
        assert!(!t.entries.iter().any(|e| e.resid == 1), "resid 1 was the oldest idle buffer");
        let mut busy = Table::new();
        for r in 1..=SLOTS as u32 {
            busy.submit(r, 1, r, false);
        }
        assert_eq!(busy.submit(5000, 1, 5000, false), None);
        assert_eq!(busy.overflow, 1);
    }

    #[test]
    fn a_dead_process_frees_its_buffers_and_no_resource_id_is_ignored() {
        let mut t = Table::new();
        t.submit(1, 1, 5, false);
        t.submit(2, 2, 6, false);
        assert_eq!(t.forget_owner(1).count_ones(), 1);
        assert!(t.entries.iter().all(|e| e.resid != 1));
        assert_eq!(t.submit(0, 1, 7, false), None);
    }

    #[test]
    fn owners_fold_both_halves() {
        assert_ne!(owner_of(0xFFFF_8000_1234_5000), owner_of(0xFFFF_8000_1234_6000));
        assert_eq!(owner_of(0x1000), owner_of(0x1000));
    }
}
