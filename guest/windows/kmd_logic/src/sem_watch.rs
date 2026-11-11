//! `NvSemWatch`: one GPU semaphore address watched end to end (the I/O half is
//! `kmd_render::ddi::sem_watch`).
//!
//! The knob `NvSemWatch` is the VA's low 32 bits, `NvSemWatchHi` the high ones (0 = off when
//! both are 0). Three registry rings of [`RING`] records, four dwords each, published by the
//! mirror pass as `NvSw<ring><field><kk>` (ring `R`, `V` or `M`, field `a`..`d`, `kk` the slot
//! = record number % [`RING`], two decimal digits), with `NvSw<ring>N` the records ever:
//!
//! * `R` releases to the VA seen in a submission (or done by a KMD executor): `a` =
//!   [`release_word`] (source, semaphore kind, node, operation, channel id), `b` the payload's low 32
//!   bits, `c` interrupt time (ms), `d` the submitter's pid (or the process key's low 32 bits).
//! * `V` changes of the value at the VA as the GPU sees it, sampled every
//!   [`SAMPLE_MS`]: `a` / `b` the value's low / high 32 bits, `c` time (ms), `d` the
//!   process key's low 32 bits.
//! * `M` CPU-mapping and paging events of the allocation holding the VA: `a` =
//!   [`map_word`] (op, allocation id), `b` / `d` the op's details ([`map_op`]), `c` time (ms).

use core::cell::UnsafeCell;
use core::sync::atomic::{AtomicU32, Ordering};

/// Records per ring.
pub const RING: usize = 16;
/// How often the value is sampled while the watch is on (ms).
pub const SAMPLE_MS: u32 = 100;
/// Native-submit segments scanned per submission, and dwords per segment.
pub const SUBMIT_SEGMENTS: u32 = 64;
pub const SUBMIT_SEG_DWORDS: u32 = 4096;

/// The watched VA from the knobs: `None` when off (both 0, or unset).
pub const fn target(lo: u32, hi: u32) -> Option<u64> {
    let lo = if lo == u32::MAX { 0 } else { lo };
    let hi = if hi == u32::MAX { 0 } else { hi };
    let va = (hi as u64) << 32 | lo as u64;
    if va == 0 {
        None
    } else {
        Some(va)
    }
}

/// Where a release (or a dropped submission) was seen.
pub mod src {
    /// A native HW-queue submission (`SubmitCommandToHwQueue`), scanned at submit.
    pub const NATIVE: u32 = 1;
    /// A copy-context channel submission (`nv_copy`), scanned from the mirror pass.
    pub const COPY_CHANNEL: u32 = 2;
    /// The copy context's CPU executor wrote it (`NvCcCpu`).
    pub const COPY_CPU: u32 = 3;
    /// Dropped: an NVDA HW-queue context on a node without a channel; completed unrun.
    pub const SOFT_DROP: u32 = 4;
    /// Dropped: an NVDA SubmitCommand context without a channel or record; completed unrun.
    pub const COPY_DROP: u32 = 5;
    pub const COUNT: usize = 6;
}

/// `R` ring word `a`: source << 28 | semaphore kind << 24 | [`ACQUIRE_BIT`] | node << 16 |
/// operation << 12 | channel id (12 bits). The operation is `native_snap::SemOp::op` (for a
/// host `SEM_EXECUTE` 1 RELEASE, 6 REDUCTION: then `b` is the reduction's operand, not the
/// value written; 0, 2..5 acquires). Node is 7 bits.
pub const fn release_word(source: u32, sem_kind: u8, node: u32, op: u8, chid: u32) -> u32 {
    (source & 0xf) << 28 | (sem_kind as u32 & 0xf) << 24 | (node & 0x7f) << 16 | (op as u32 & 0xf) << 12 | (chid & 0xfff)
}

/// Bit 23 of the `R` ring word: the record is an acquire of the VA (`b` the value waited
/// for), not a release.
pub const ACQUIRE_BIT: u32 = 1 << 23;

/// [`release_word`] for a release or an acquire.
pub const fn op_word(source: u32, sem_kind: u8, node: u32, op: u8, chid: u32, acquire: bool) -> u32 {
    release_word(source, sem_kind, node, op, chid) | if acquire { ACQUIRE_BIT } else { 0 }
}

/// One piece of work to scan for the watched VA, queued at submit time (any IRQL, usually
/// DISPATCH) and scanned by the mirror pass at PASSIVE.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ScanRec {
    /// [`scan_kind`].
    pub kind: u8,
    /// [`src`].
    pub source: u8,
    pub node: u8,
    pub chid: u32,
    pub pid: u32,
    pub t_ms: u32,
    /// The process key the VAs belong to.
    pub process: u64,
    /// `GPFIFO`: the ring's GPU VA; `SEGMENT`: the segment's.
    pub va: u64,
    /// `GPFIFO`: the first entry index; `SEGMENT`: its dwords.
    pub a: u32,
    /// `GPFIFO`: the index after the last; `SEGMENT`: the submission's sequence (low 32).
    pub b: u32,
    /// `GPFIFO`: the ring's entries; `SEGMENT`: the copy channel slot.
    pub c: u32,
}

pub mod scan_kind {
    /// A native HW-queue submission: GPFIFO entries `[a, b)` of the ring at `va` (`c` entries).
    pub const GPFIFO: u8 = 1;
    /// One pushbuffer segment (a copy-context submission's).
    pub const SEGMENT: u8 = 2;
}

/// Records the scan queue holds.
pub const SCAN_QUEUE: usize = 1024;
/// Segments of one copy-context submission queued at most.
pub const COPY_SEGMENTS: usize = 64;
/// Records the mirror pass scans per pass at most (the rest stay for the next pass).
pub const SCAN_PER_PASS: usize = 512;
/// A pass stops scanning after this long (ms); the rest stay queued.
pub const SCAN_PASS_MS: u32 = 40;

struct Slot {
    /// The slot's sequence minus its index (so the all-zero array is the initial state).
    seq: AtomicU32,
    rec: UnsafeCell<ScanRec>,
}

/// A bounded lock-free multi-producer, single-consumer queue of [`ScanRec`]s (Vyukov's
/// bounded queue): producers at any IRQL never spin on a lock, a full queue refuses
/// ([`ScanQueue::push`] `false`), the single consumer (the mirror thread) pops in order.
pub struct ScanQueue<const N: usize> {
    slots: [Slot; N],
    head: AtomicU32,
    tail: AtomicU32,
}

// SAFETY: a slot's record is written only by the producer that claimed its position (the
// head CAS) before the release store of its sequence, and read only by the consumer after
// the acquire load saw that sequence.
unsafe impl<const N: usize> Sync for ScanQueue<N> {}

impl<const N: usize> Default for ScanQueue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> ScanQueue<N> {
    const ASSERT: () = assert!(N.is_power_of_two() && N <= 1 << 16);

    pub const fn new() -> Self {
        #[allow(clippy::let_unit_value)]
        let _ = Self::ASSERT;
        const EMPTY: ScanRec = ScanRec {
            kind: 0,
            source: 0,
            node: 0,
            chid: 0,
            pid: 0,
            t_ms: 0,
            process: 0,
            va: 0,
            a: 0,
            b: 0,
            c: 0,
        };
        Self {
            slots: [const { Slot { seq: AtomicU32::new(0), rec: UnsafeCell::new(EMPTY) } }; N],
            head: AtomicU32::new(0),
            tail: AtomicU32::new(0),
        }
    }

    fn seq(&self, i: usize) -> u32 {
        self.slots[i].seq.load(Ordering::Acquire).wrapping_add(i as u32)
    }

    fn set_seq(&self, i: usize, seq: u32) {
        self.slots[i].seq.store(seq.wrapping_sub(i as u32), Ordering::Release);
    }

    /// Queue `rec`; `false` when full. Any thread, any IRQL.
    pub fn push(&self, rec: ScanRec) -> bool {
        let mut pos = self.head.load(Ordering::Relaxed);
        loop {
            let i = pos as usize % N;
            let dif = self.seq(i).wrapping_sub(pos) as i32;
            if dif == 0 {
                match self.head.compare_exchange_weak(pos, pos.wrapping_add(1), Ordering::Relaxed, Ordering::Relaxed) {
                    Ok(_) => {
                        // SAFETY: this producer owns position `pos` until the store below.
                        unsafe { *self.slots[i].rec.get() = rec };
                        self.set_seq(i, pos.wrapping_add(1));
                        return true;
                    }
                    Err(now) => pos = now,
                }
            } else if dif < 0 {
                return false;
            } else {
                pos = self.head.load(Ordering::Relaxed);
            }
        }
    }

    /// The oldest record, if one is complete. The single consumer only.
    pub fn pop(&self) -> Option<ScanRec> {
        let pos = self.tail.load(Ordering::Relaxed);
        let i = pos as usize % N;
        if self.seq(i) != pos.wrapping_add(1) {
            return None;
        }
        // SAFETY: the producer published the record (the acquire in `seq`); nobody writes the
        // slot again until the store below hands it back.
        let rec = unsafe { *self.slots[i].rec.get() };
        self.set_seq(i, pos.wrapping_add(N as u32));
        self.tail.store(pos.wrapping_add(1), Ordering::Relaxed);
        Some(rec)
    }
}

/// CPU-mapping / paging operations of the watched allocation (`M` ring).
pub mod map_op {
    /// `MapCpuHostAperture` mapped it: `b` aperture offset >> 12, `d` guest physical >> 12.
    pub const APERTURE_MAP: u32 = 1;
    /// `MapCpuHostAperture` failed: `b` aperture offset >> 12, `d` the error + 1.
    pub const APERTURE_MAP_FAIL: u32 = 2;
    /// `UnmapCpuHostAperture`: `b` aperture offset >> 12, `d` 0 ok / error + 1.
    pub const APERTURE_UNMAP: u32 = 3;
    /// A paging content operation: `b` the `pg_check::op_word`, `d` 0.
    pub const PAGING: u32 = 4;
    /// Destroyed.
    pub const DESTROY: u32 = 5;
}

/// `M` ring word `a`: op << 24 | allocation id (24 bits).
pub const fn map_word(op: u32, id: u32) -> u32 {
    (op & 0xff) << 24 | (id & 0xff_ffff)
}

/// A ring of four-dword records; `n` records ever.
#[derive(Clone, Copy)]
pub struct Ring {
    pub recs: [[u32; 4]; RING],
    pub n: u32,
    /// Records already published.
    pub published: u32,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    pub const fn new() -> Self {
        Self {
            recs: [[0; 4]; RING],
            n: 0,
            published: 0,
        }
    }

    pub fn push(&mut self, rec: [u32; 4]) {
        self.recs[self.n as usize % RING] = rec;
        self.n = self.n.wrapping_add(1);
    }

    /// The records not yet published (at most the ring), as (slot, record); marks them
    /// published.
    pub fn take_new(&mut self, out: &mut [(usize, [u32; 4]); RING]) -> usize {
        let pending = self.n.wrapping_sub(self.published).min(RING as u32);
        let first = self.n.wrapping_sub(pending);
        for k in 0..pending {
            let i = first.wrapping_add(k);
            let slot = i as usize % RING;
            out[k as usize] = (slot, self.recs[slot]);
        }
        self.published = self.n;
        pending as usize
    }
}

/// The registry name of one ring field: `NvSw` + ring + field + two digits.
pub const fn name(ring: u8, field: u8, slot: usize) -> [u8; 8] {
    [b'N', b'v', b'S', b'w', ring, field, b'0' + (slot / 10 % 10) as u8, b'0' + (slot % 10) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knobs() {
        assert_eq!(target(0, 0), None);
        assert_eq!(target(u32::MAX, u32::MAX), None);
        assert_eq!(target(0x852d070, 0), Some(0x852d070));
        assert_eq!(target(0x852d070, u32::MAX), Some(0x852d070));
        assert_eq!(target(0x1000, 1), Some(0x1_0000_1000));
    }

    #[test]
    fn words_and_names() {
        assert_eq!(release_word(src::NATIVE, 1, 0, 1, 0x30), 0x1100_1030);
        assert_eq!(release_word(src::COPY_DROP, 0, 3, 0, 0x1234), 0x5003_0234);
        assert_eq!(release_word(src::NATIVE, 1, 2, 6, 0x31), 0x1102_6031);
        assert_eq!(map_word(map_op::APERTURE_MAP, 2414), 0x0100_096e);
        assert_eq!(&name(b'R', b'a', 7), b"NvSwRa07");
        assert_eq!(&name(b'V', b'c', 15), b"NvSwVc15");
    }

    #[test]
    fn acquire_words() {
        let w = op_word(src::COPY_CHANNEL, 1, 3, 0, 0x41, true);
        assert_eq!(w & ACQUIRE_BIT, ACQUIRE_BIT);
        assert_eq!(w & !ACQUIRE_BIT, release_word(src::COPY_CHANNEL, 1, 3, 0, 0x41));
        assert_eq!(op_word(src::NATIVE, 1, 0, 1, 0x30, false), 0x1100_1030);
        // Node is 7 bits: the flag never comes from a node.
        assert_eq!(release_word(src::NATIVE, 0, 0xff, 0, 0) & ACQUIRE_BIT, 0);
    }

    fn rec(i: u32) -> ScanRec {
        ScanRec { kind: scan_kind::SEGMENT, a: i, va: 0x1000 + u64::from(i), ..ScanRec::default() }
    }

    #[test]
    fn scan_queue_fifo_and_full() {
        let q: ScanQueue<8> = ScanQueue::new();
        assert_eq!(q.pop(), None);
        for i in 0..8 {
            assert!(q.push(rec(i)));
        }
        assert!(!q.push(rec(99)));
        for i in 0..5 {
            assert_eq!(q.pop().map(|r| r.a), Some(i));
        }
        for i in 8..13 {
            assert!(q.push(rec(i)));
        }
        assert!(!q.push(rec(99)));
        for i in 5..13 {
            assert_eq!(q.pop().map(|r| r.a), Some(i));
        }
        assert_eq!(q.pop(), None);
        // Many laps (positions wrap the slot sequences).
        for lap in 0..10_000u32 {
            assert!(q.push(rec(lap)));
            assert_eq!(q.pop().map(|r| r.a), Some(lap));
        }
    }

    #[test]
    fn scan_queue_many_producers() {
        extern crate std;
        use std::sync::Arc;
        let q: Arc<ScanQueue<64>> = Arc::new(ScanQueue::new());
        let producers = 4u32;
        let per = 5_000u32;
        let mut handles = std::vec::Vec::new();
        for p in 0..producers {
            let q = q.clone();
            handles.push(std::thread::spawn(move || {
                let mut refused = 0u32;
                for i in 0..per {
                    let r = ScanRec { a: i, b: p, kind: scan_kind::GPFIFO, ..ScanRec::default() };
                    while !q.push(r) {
                        refused += 1;
                        std::thread::yield_now();
                    }
                }
                refused
            }));
        }
        let mut next = std::vec![0u32; producers as usize];
        let mut got = 0u32;
        while got < producers * per {
            if let Some(r) = q.pop() {
                // Each producer's records arrive in its order, none twice, none lost.
                assert_eq!(r.a, next[r.b as usize]);
                next[r.b as usize] += 1;
                got += 1;
            } else {
                std::thread::yield_now();
            }
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(q.pop(), None);
        assert!(next.iter().all(|n| *n == per));
    }

    #[test]
    fn ring_publishes_each_record_once() {
        let mut r = Ring::new();
        let mut out = [(0usize, [0u32; 4]); RING];
        assert_eq!(r.take_new(&mut out), 0);
        r.push([1, 0, 0, 0]);
        r.push([2, 0, 0, 0]);
        assert_eq!(r.take_new(&mut out), 2);
        assert_eq!((out[0].0, out[0].1[0], out[1].0, out[1].1[0]), (0, 1, 1, 2));
        assert_eq!(r.take_new(&mut out), 0);
        // 20 more: only the ring's last 16 are published, oldest first.
        for i in 0..20u32 {
            r.push([100 + i, 0, 0, 0]);
        }
        assert_eq!(r.take_new(&mut out), RING);
        assert_eq!(out[0].1[0], 104);
        assert_eq!(out[0].0, 6);
        assert_eq!(out[RING - 1].1[0], 119);
        assert_eq!(r.n, 22);
    }
}
