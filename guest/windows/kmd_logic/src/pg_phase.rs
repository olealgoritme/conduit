//! Where a page-table leaf update spends its time (`PgPh*` / `PgPv*` counters).
//!
//! The I/O half is `kmd_render` `virtio/native_memory.rs`. A leaf update runs a
//! [`Stopwatch`]: the update says which phase it is in ([`Phase`]), and the time
//! between two switches goes to the phase it was in. Waits and host round trips
//! that happen deep inside helper functions (the native-memory mutex, the STATE
//! spinlock, every RM round trip, the OS-descriptor lookup, the page-run table)
//! are added by those helpers to the calling thread's [`Slots`] entry; at each
//! switch the stopwatch moves what they added in the segment out of the current
//! phase into their own ([`Acc`]). So the phases of one update add up to its
//! wall time, and a wait is counted where it happened, not where it was called
//! from.
//!
//! [`PhaseStats`] keeps, over all updates, every phase's total and longest
//! segment, the longest update and its own breakdown ("which phase made the
//! longest update long"). [`KindStats`] keeps how long each kind of operation
//! held the native-memory mutex (the holder a waiter waited for).
//!
//! Everything is atomics (legal at any IRQL); nothing here writes the registry.
//! Times are 100-ns ticks in, microseconds out.

use core::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

/// The phases of a leaf update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Phase {
    /// The unchanged check before anything is locked (`NvPerfMem` 0x40).
    Skip = 0,
    /// Waiting for the native-memory mutex (every acquisition of the update).
    Mutex = 1,
    /// Waiting for the STATE spinlock.
    Spin = 2,
    /// Dropping what other allocations / foreign ranges mapped in the range.
    Evict = 3,
    /// RM's idle-channels quiesce before a live replacement.
    Quiesce = 4,
    /// Unmapping the allocation's own old mappings in the range.
    Unmap = 5,
    /// The process's VA space (made on first use).
    Space = 6,
    /// OS-descriptor lookup / handle mint (`desc_cache`).
    Lookup = 7,
    /// Page-run table of a new descriptor (`nvrm::page_run_table`).
    Table = 8,
    /// Building the batch and reading its replies (CPU).
    Build = 9,
    /// Host RM round trips (single calls and batches).
    Rt = 10,
    /// Recording the new mappings (STATE bookkeeping, sorting, the index).
    Book = 11,
    /// The rest: argument checks, range arithmetic, allocations.
    Other = 12,
}

pub const PHASES: usize = 13;

impl Phase {
    pub const ALL: [Phase; PHASES] = [
        Phase::Skip,
        Phase::Mutex,
        Phase::Spin,
        Phase::Evict,
        Phase::Quiesce,
        Phase::Unmap,
        Phase::Space,
        Phase::Lookup,
        Phase::Table,
        Phase::Build,
        Phase::Rt,
        Phase::Book,
        Phase::Other,
    ];

    /// The value-name part after the prefix (`PgPh` + this + `""` / `Mx` / `W`).
    pub const fn name(self) -> &'static [u8] {
        match self {
            Phase::Skip => b"Skip",
            Phase::Mutex => b"Mtx",
            Phase::Spin => b"Spin",
            Phase::Evict => b"Evict",
            Phase::Quiesce => b"Qui",
            Phase::Unmap => b"Unmap",
            Phase::Space => b"Space",
            Phase::Lookup => b"Lookup",
            Phase::Table => b"Table",
            Phase::Build => b"Build",
            Phase::Rt => b"Rt",
            Phase::Book => b"Book",
            Phase::Other => b"Other",
        }
    }
}

/// What helpers add to the calling thread's slot, moved out of the current
/// phase at the next switch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Acc {
    Rt = 0,
    Mutex = 1,
    Spin = 2,
    Lookup = 3,
    Table = 4,
}

pub const ACCS: usize = 5;

impl Acc {
    pub const ALL: [Acc; ACCS] = [Acc::Rt, Acc::Mutex, Acc::Spin, Acc::Lookup, Acc::Table];

    pub const fn phase(self) -> Phase {
        match self {
            Acc::Rt => Phase::Rt,
            Acc::Mutex => Phase::Mutex,
            Acc::Spin => Phase::Spin,
            Acc::Lookup => Phase::Lookup,
            Acc::Table => Phase::Table,
        }
    }
}

/// Per-thread accumulators of the updates in flight: a thread claims a slot for
/// one update; helpers add to the slot of the thread they run on (a scan of `N`
/// atomics, nothing when no slot is claimed). Thread 0 is never a thread.
pub struct Slots<const N: usize> {
    thread: [AtomicUsize; N],
    acc: [[AtomicU64; ACCS]; N],
    claimed: AtomicU32,
}

impl<const N: usize> Slots<N> {
    pub const fn new() -> Self {
        Self {
            thread: [const { AtomicUsize::new(0) }; N],
            acc: [const { [const { AtomicU64::new(0) }; ACCS] }; N],
            claimed: AtomicU32::new(0),
        }
    }

    /// A slot for `thread`'s update, its accumulators zeroed; `None` when all are
    /// taken (or `thread` is 0, or it holds one already: a nested update).
    pub fn claim(&self, thread: usize) -> Option<usize> {
        if thread == 0 || self.find(thread).is_some() {
            return None;
        }
        for (i, owner) in self.thread.iter().enumerate() {
            if owner
                .compare_exchange(0, usize::MAX, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                for a in &self.acc[i] {
                    a.store(0, Ordering::Relaxed);
                }
                owner.store(thread, Ordering::Release);
                self.claimed.fetch_add(1, Ordering::AcqRel);
                return Some(i);
            }
        }
        None
    }

    pub fn release(&self, slot: usize) {
        if let Some(owner) = self.thread.get(slot) {
            if owner.swap(0, Ordering::AcqRel) != 0 {
                self.claimed.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }

    /// The slot `thread` holds.
    pub fn find(&self, thread: usize) -> Option<usize> {
        if thread == 0 || self.claimed.load(Ordering::Acquire) == 0 {
            return None;
        }
        self.thread
            .iter()
            .position(|t| t.load(Ordering::Acquire) == thread)
    }

    /// Whether any update is being timed (the helpers' fast path).
    pub fn any(&self) -> bool {
        self.claimed.load(Ordering::Relaxed) != 0
    }

    /// Add `ticks` to `acc` of `thread`'s slot, if it has one.
    pub fn add(&self, thread: usize, acc: Acc, ticks: u64) {
        if let Some(i) = self.find(thread) {
            self.acc[i][acc as usize].fetch_add(ticks, Ordering::Relaxed);
        }
    }

    /// The slot's accumulators now.
    pub fn read(&self, slot: usize) -> [u64; ACCS] {
        let mut out = [0; ACCS];
        if let Some(acc) = self.acc.get(slot) {
            for (o, a) in out.iter_mut().zip(acc) {
                *o = a.load(Ordering::Relaxed);
            }
        }
        out
    }

    /// A timed scope of `acc` on `thread` (the descriptor lookup, the table):
    /// what the slot held at its start, for [`Self::scope_end`].
    pub fn scope_begin(&self, thread: usize) -> Option<(usize, [u64; ACCS])> {
        self.find(thread).map(|i| (i, self.read(i)))
    }

    /// End a scope that took `wall` ticks: `acc` gets the wall time minus what
    /// the other accumulators gained inside it (a spin wait inside the lookup
    /// stays a spin wait).
    pub fn scope_end(&self, begun: Option<(usize, [u64; ACCS])>, acc: Acc, wall: u64) {
        let Some((i, before)) = begun else {
            return;
        };
        let now = self.read(i);
        let nested: u64 = Acc::ALL
            .iter()
            .filter(|a| **a != acc)
            .map(|a| now[*a as usize].saturating_sub(before[*a as usize]))
            .sum();
        self.acc[i][acc as usize].fetch_add(wall.saturating_sub(nested), Ordering::Relaxed);
    }
}

impl<const N: usize> Default for Slots<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// One update's time per phase (ticks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Breakdown {
    pub ticks: [u64; PHASES],
}

impl Breakdown {
    pub fn total(&self) -> u64 {
        self.ticks.iter().sum()
    }

    /// The phase with the most time (the first of equals).
    pub fn dominant(&self) -> Phase {
        let mut best = 0;
        for (i, t) in self.ticks.iter().enumerate() {
            if *t > self.ticks[best] {
                best = i;
            }
        }
        Phase::ALL[best]
    }
}

/// Times one update: the current phase, and where the segment started.
#[derive(Clone, Copy, Debug)]
pub struct Stopwatch {
    phase: Phase,
    mark: u64,
    acc_mark: [u64; ACCS],
    pub out: Breakdown,
}

impl Stopwatch {
    /// Start in `phase` at `now`, with the slot's accumulators `acc` (zero for a
    /// thread without a slot).
    pub fn start(phase: Phase, now: u64, acc: [u64; ACCS]) -> Self {
        Self {
            phase,
            mark: now,
            acc_mark: acc,
            out: Breakdown::default(),
        }
    }

    pub fn phase(&self) -> Phase {
        self.phase
    }

    /// Close the segment at `now` and continue in `next`. The segment's wall
    /// time goes to the current phase, minus what the accumulators gained in
    /// it, which goes to their phases (never more than the segment: a clock
    /// that went backwards or an over-count is cut, not invented).
    pub fn switch(&mut self, next: Phase, now: u64, acc: [u64; ACCS]) {
        let mut left = now.saturating_sub(self.mark);
        for a in Acc::ALL {
            let i = a as usize;
            let gained = acc[i].saturating_sub(self.acc_mark[i]).min(left);
            left -= gained;
            self.out.ticks[a.phase() as usize] += gained;
        }
        self.out.ticks[self.phase as usize] += left;
        self.phase = next;
        self.mark = now;
        self.acc_mark = acc;
    }

    /// Close the last segment: the update's breakdown.
    pub fn finish(mut self, now: u64, acc: [u64; ACCS]) -> Breakdown {
        let phase = self.phase;
        self.switch(phase, now, acc);
        self.out
    }
}

/// Updates at least this long count as slow (`<prefix>Slow`).
pub const SLOW_TICKS: u64 = 100_000; // 10 ms

fn us(ticks: u64) -> u32 {
    u32::try_from(ticks / 10).unwrap_or(u32::MAX)
}

/// Phase totals over every update, the longest segment per phase, and the
/// longest update with its own breakdown.
pub struct PhaseStats {
    updates: AtomicU32,
    slow: AtomicU32,
    total_us: [AtomicU32; PHASES],
    max_us: [AtomicU32; PHASES],
    worst_us: AtomicU32,
    at_worst_us: [AtomicU32; PHASES],
    /// Updates that could not be timed in phases (no free slot).
    untimed: AtomicU32,
}

impl PhaseStats {
    pub const fn new() -> Self {
        Self {
            updates: AtomicU32::new(0),
            slow: AtomicU32::new(0),
            total_us: [const { AtomicU32::new(0) }; PHASES],
            max_us: [const { AtomicU32::new(0) }; PHASES],
            worst_us: AtomicU32::new(0),
            at_worst_us: [const { AtomicU32::new(0) }; PHASES],
            untimed: AtomicU32::new(0),
        }
    }

    /// One timed update.
    pub fn note(&self, b: &Breakdown) {
        self.updates.fetch_add(1, Ordering::Relaxed);
        let total = b.total();
        if total >= SLOW_TICKS {
            self.slow.fetch_add(1, Ordering::Relaxed);
        }
        for (i, t) in b.ticks.iter().enumerate() {
            let v = us(*t);
            self.total_us[i].fetch_add(v, Ordering::Relaxed);
            self.max_us[i].fetch_max(v, Ordering::Relaxed);
        }
        let total = us(total);
        // The longest update keeps its breakdown. Two record-setting updates at
        // once may mix their phases; the next longer one writes it whole again.
        if self.worst_us.fetch_max(total, Ordering::AcqRel) < total {
            for (i, t) in b.ticks.iter().enumerate() {
                self.at_worst_us[i].store(us(*t), Ordering::Relaxed);
            }
        }
    }

    pub fn note_untimed(&self) {
        self.untimed.fetch_add(1, Ordering::Relaxed);
    }

    pub fn updates(&self) -> u32 {
        self.updates.load(Ordering::Relaxed)
    }
    pub fn slow(&self) -> u32 {
        self.slow.load(Ordering::Relaxed)
    }
    pub fn untimed(&self) -> u32 {
        self.untimed.load(Ordering::Relaxed)
    }
    pub fn total_us(&self, p: Phase) -> u32 {
        self.total_us[p as usize].load(Ordering::Relaxed)
    }
    pub fn max_us(&self, p: Phase) -> u32 {
        self.max_us[p as usize].load(Ordering::Relaxed)
    }
    pub fn worst_us(&self) -> u32 {
        self.worst_us.load(Ordering::Relaxed)
    }
    pub fn at_worst_us(&self, p: Phase) -> u32 {
        self.at_worst_us[p as usize].load(Ordering::Relaxed)
    }
    /// The phase that took most of the longest update.
    pub fn worst_phase(&self) -> Phase {
        let mut b = Breakdown::default();
        for p in Phase::ALL {
            b.ticks[p as usize] = u64::from(self.at_worst_us(p));
        }
        b.dominant()
    }
}

impl Default for PhaseStats {
    fn default() -> Self {
        Self::new()
    }
}

/// Who holds the native-memory mutex (`PgHo*`, `PgNmWait*`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Kind {
    None = 0,
    Leaf = 1,
    System = 2,
    Foreign = 3,
    Create = 4,
    Destroy = 5,
    Process = 6,
    CpuRange = 7,
    ApertureMap = 8,
    ApertureUnmap = 9,
    WithIo = 10,
    Client = 11,
    Forget = 12,
    /// `native_memory::gpu_paging`: the RM map / unmap calls of a copy-engine paging op.
    GpuPaging = 13,
}

pub const KINDS: usize = 14;

/// Mutex holds per [`Kind`]: count, total and longest hold; and the waits for
/// it: count, total, longest, and the kind that held it when the longest began.
pub struct KindStats {
    held_n: [AtomicU32; KINDS],
    held_us: [AtomicU32; KINDS],
    held_max_us: [AtomicU32; KINDS],
    wait_n: AtomicU32,
    wait_us: AtomicU32,
    wait_max_us: AtomicU32,
    wait_max_by: AtomicU32,
}

impl KindStats {
    pub const fn new() -> Self {
        Self {
            held_n: [const { AtomicU32::new(0) }; KINDS],
            held_us: [const { AtomicU32::new(0) }; KINDS],
            held_max_us: [const { AtomicU32::new(0) }; KINDS],
            wait_n: AtomicU32::new(0),
            wait_us: AtomicU32::new(0),
            wait_max_us: AtomicU32::new(0),
            wait_max_by: AtomicU32::new(0),
        }
    }

    pub fn held(&self, kind: u32, ticks: u64) {
        let k = (kind as usize).min(KINDS - 1);
        let v = us(ticks);
        self.held_n[k].fetch_add(1, Ordering::Relaxed);
        self.held_us[k].fetch_add(v, Ordering::Relaxed);
        self.held_max_us[k].fetch_max(v, Ordering::Relaxed);
    }

    /// A wait of `ticks` that began while a `holder` held the mutex (0: free or
    /// unknown). Waits under a microsecond are not counted.
    pub fn waited(&self, holder: u32, ticks: u64) {
        let v = us(ticks);
        if v == 0 {
            return;
        }
        self.wait_n.fetch_add(1, Ordering::Relaxed);
        self.wait_us.fetch_add(v, Ordering::Relaxed);
        if self.wait_max_us.fetch_max(v, Ordering::AcqRel) < v {
            self.wait_max_by.store(holder, Ordering::Relaxed);
        }
    }

    /// `(count, total us, longest us)` of `kind`'s holds.
    pub fn held_of(&self, kind: usize) -> (u32, u32, u32) {
        let k = kind.min(KINDS - 1);
        (
            self.held_n[k].load(Ordering::Relaxed),
            self.held_us[k].load(Ordering::Relaxed),
            self.held_max_us[k].load(Ordering::Relaxed),
        )
    }

    /// `(count, total us, longest us, holder kind at the longest)`.
    pub fn waits(&self) -> (u32, u32, u32, u32) {
        (
            self.wait_n.load(Ordering::Relaxed),
            self.wait_us.load(Ordering::Relaxed),
            self.wait_max_us.load(Ordering::Relaxed),
            self.wait_max_by.load(Ordering::Relaxed),
        )
    }
}

impl Default for KindStats {
    fn default() -> Self {
        Self::new()
    }
}

/// The longest wait for / hold of a lock and the source line that had it.
pub struct LineMax {
    total_us: AtomicU32,
    max_us: AtomicU32,
    line: AtomicU32,
}

impl LineMax {
    pub const fn new() -> Self {
        Self {
            total_us: AtomicU32::new(0),
            max_us: AtomicU32::new(0),
            line: AtomicU32::new(0),
        }
    }

    pub fn note(&self, ticks: u64, line: u32) {
        let v = us(ticks);
        if v == 0 {
            return;
        }
        self.total_us.fetch_add(v, Ordering::Relaxed);
        if self.max_us.fetch_max(v, Ordering::AcqRel) < v {
            self.line.store(line, Ordering::Relaxed);
        }
    }

    /// `(total us, longest us, its line)`.
    pub fn get(&self) -> (u32, u32, u32) {
        (
            self.total_us.load(Ordering::Relaxed),
            self.max_us.load(Ordering::Relaxed),
            self.line.load(Ordering::Relaxed),
        )
    }
}

impl Default for LineMax {
    fn default() -> Self {
        Self::new()
    }
}

/// `prefix` + `middle` + `suffix` as a value name (at most 14 bytes, cut).
pub fn name(prefix: &[u8], middle: &[u8], suffix: &[u8]) -> ([u8; 14], usize) {
    let mut out = [0u8; 14];
    let mut n = 0;
    for b in prefix.iter().chain(middle).chain(suffix) {
        if n == out.len() {
            break;
        }
        out[n] = *b;
        n += 1;
    }
    (out, n)
}

/// One hex digit (0-15) for per-kind names.
pub const fn hex(k: usize) -> u8 {
    let k = (k & 0xf) as u8;
    if k < 10 {
        b'0' + k
    } else {
        b'a' + k - 10
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(rt: u64, mutex: u64, spin: u64, lookup: u64, table: u64) -> [u64; ACCS] {
        [rt, mutex, spin, lookup, table]
    }

    #[test]
    fn segments_go_to_their_phase_and_waits_to_theirs() {
        let mut w = Stopwatch::start(Phase::Other, 1_000, acc(0, 0, 0, 0, 0));
        // 300 ticks of checks, then the mutex: 5_000 waited inside the lock call.
        w.switch(Phase::Unmap, 6_300, acc(0, 5_000, 0, 0, 0));
        // Unmap: 2_000 wall, 1_500 of it a round trip, 100 a spin wait.
        w.switch(Phase::Build, 8_300, acc(1_500, 5_000, 100, 0, 0));
        // Build: 1_000 wall, 200 lookup, 300 table, 400 round trip.
        let b = w.finish(9_300, acc(1_900, 5_000, 100, 200, 300));
        assert_eq!(b.ticks[Phase::Other as usize], 300);
        assert_eq!(b.ticks[Phase::Mutex as usize], 5_000);
        assert_eq!(b.ticks[Phase::Unmap as usize], 400);
        assert_eq!(b.ticks[Phase::Rt as usize], 1_900);
        assert_eq!(b.ticks[Phase::Spin as usize], 100);
        assert_eq!(b.ticks[Phase::Lookup as usize], 200);
        assert_eq!(b.ticks[Phase::Table as usize], 300);
        assert_eq!(b.ticks[Phase::Build as usize], 100);
        assert_eq!(b.total(), 8_300, "the phases add up to the wall time");
        assert_eq!(b.dominant(), Phase::Mutex);
    }

    #[test]
    fn an_over_count_is_cut_not_invented() {
        let mut w = Stopwatch::start(Phase::Evict, 0, acc(0, 0, 0, 0, 0));
        // The accumulators claim more than the segment's wall time.
        w.switch(Phase::Book, 100, acc(80, 0, 50, 0, 0));
        let b = w.finish(90, acc(80, 0, 50, 0, 0)); // the clock went backwards
        assert_eq!(b.ticks[Phase::Rt as usize], 80);
        assert_eq!(b.ticks[Phase::Spin as usize], 20);
        assert_eq!(b.ticks[Phase::Evict as usize], 0);
        assert_eq!(b.total(), 100);
    }

    #[test]
    fn slots_are_per_thread() {
        let s: Slots<2> = Slots::new();
        assert!(!s.any());
        s.add(7, Acc::Rt, 5); // no slot: dropped
        let a = s.claim(7).unwrap();
        assert_eq!(s.claim(7), None, "one slot per thread");
        let b = s.claim(9).unwrap();
        assert_eq!(s.claim(11), None, "full");
        assert_eq!(s.claim(0), None);
        s.add(7, Acc::Rt, 5);
        s.add(9, Acc::Spin, 3);
        s.add(11, Acc::Rt, 100);
        assert_eq!(s.read(a), acc(5, 0, 0, 0, 0));
        assert_eq!(s.read(b), acc(0, 0, 3, 0, 0));
        s.release(a);
        let c = s.claim(11).unwrap();
        assert_eq!(c, a);
        assert_eq!(s.read(c), [0; ACCS], "a new claim starts at zero");
        s.release(b);
        s.release(c);
        s.release(c);
        assert!(!s.any());
    }

    #[test]
    fn a_scope_leaves_nested_waits_where_they_are() {
        let s: Slots<1> = Slots::new();
        s.claim(3).unwrap();
        let begun = s.scope_begin(3);
        s.add(3, Acc::Spin, 40);
        s.scope_end(begun, Acc::Lookup, 100);
        assert_eq!(s.read(0), acc(0, 0, 40, 60, 0));
        s.scope_end(s.scope_begin(4), Acc::Table, 100);
        assert_eq!(s.read(0), acc(0, 0, 40, 60, 0), "no slot, no time");
    }

    #[test]
    fn stats_keep_the_longest_update_whole() {
        let st = PhaseStats::new();
        let mut a = Breakdown::default();
        a.ticks[Phase::Rt as usize] = 50_000;
        a.ticks[Phase::Book as usize] = 10;
        st.note(&a);
        let mut b = Breakdown::default();
        b.ticks[Phase::Mutex as usize] = 12_000_000; // 1.2 s
        b.ticks[Phase::Rt as usize] = 1_000;
        st.note(&b);
        let mut c = Breakdown::default();
        c.ticks[Phase::Rt as usize] = 60_000;
        st.note(&c);
        assert_eq!(st.updates(), 3);
        assert_eq!(st.slow(), 1);
        assert_eq!(st.total_us(Phase::Rt), 5_000 + 100 + 6_000);
        assert_eq!(st.max_us(Phase::Rt), 6_000);
        assert_eq!(st.worst_us(), 1_200_100);
        assert_eq!(st.at_worst_us(Phase::Mutex), 1_200_000);
        assert_eq!(st.at_worst_us(Phase::Rt), 100);
        assert_eq!(st.at_worst_us(Phase::Book), 0, "the earlier breakdown is gone");
        assert_eq!(st.worst_phase(), Phase::Mutex);
    }

    #[test]
    fn kind_stats_name_the_holder_of_the_longest_wait() {
        let k = KindStats::new();
        k.held(Kind::CpuRange as u32, 12_000_000);
        k.held(Kind::CpuRange as u32, 10_000);
        k.held(99, 10); // clamped into the last kind
        assert_eq!(k.held_of(Kind::CpuRange as usize), (2, 1_201_000, 1_200_000));
        assert_eq!(k.held_of(KINDS - 1).0, 1);
        k.waited(Kind::Destroy as u32, 5); // under a microsecond
        k.waited(Kind::Destroy as u32, 20_000);
        k.waited(Kind::CpuRange as u32, 11_000_000);
        k.waited(Kind::Leaf as u32, 30_000);
        assert_eq!(k.waits(), (3, 2_000 + 1_100_000 + 3_000, 1_100_000, Kind::CpuRange as u32));
    }

    #[test]
    fn line_max_and_names() {
        let l = LineMax::new();
        l.note(5, 10);
        l.note(500, 20);
        l.note(300, 30);
        assert_eq!(l.get(), (80, 50, 20));
        let (n, len) = name(b"PgPh", Phase::Lookup.name(), b"Mx");
        assert_eq!(&n[..len], b"PgPhLookupMx");
        let (_, len) = name(b"PgPhLongPrefix", b"Lookup", b"Mx");
        assert_eq!(len, 14);
        assert_eq!(hex(3), b'3');
        assert_eq!(hex(12), b'c');
        for p in Phase::ALL {
            assert!(4 + p.name().len() + 2 <= 14, "{:?} fits", p);
        }
    }
}
