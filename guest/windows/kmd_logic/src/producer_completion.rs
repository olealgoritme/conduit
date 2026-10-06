//! Allocation-scoped producer completion. The KMD serializes this production
//! state with publication, retirement and event registration under one lock.
//! IDs below are resolved, retained dxgkrnl allocations, never resource IDs.
//! Storage is reserved at PASSIVE; every operation after `new` is allocation-free.
//!
//! # Completion rule
//!
//! A stream value is a position on a timeline semaphore: it signals monotonically,
//! so a completed value `C` proves every earlier value of the same stream. The
//! host retire path reports the stream's cumulative watermark (`Progress::completed`,
//! a running maximum), not the value of the submission that just came back. So
//! [`Table::complete`] completes every pending entry of that stream whose value is
//! `<= C` (serial-number comparison, see [`reached`]); matching only `== C` strands
//! the entries a batched, rejected-as-stale or out-of-order retirement skipped, and
//! because an allocation completes through a linked prefix, one stranded entry
//! holds every later entry of its allocation in the table until it is full.
//!
//! The table also remembers the highest completed value of each stream it has
//! published on (`Mark`), so a publication that arrives after its retirement
//! completes at once, independently of the caller's `already_complete` proof.
//! Marks and writer slots are freed with their stream (`fail_stream`, `reset`),
//! writer slots also when their last entry leaves the table.

extern crate alloc;
use alloc::vec::Vec;

pub const LIVE: u32 = 0;
pub const CANCELLED: u32 = 1;
pub const FAILED: u32 = 2;
pub const REMOVED: u32 = 3;
const NONE: usize = usize::MAX;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Key {
    pub slot: u32,
    pub generation: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub generation: u64,
    pub announced: u64,
    pub completed: u64,
    pub status: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Capacity,
    Terminal(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Predicate {
    Ready,
    Pending,
    Terminal(u32),
}

#[derive(Clone, Copy)]
struct Open {
    pointer: usize,
    process: usize,
    key: Key,
}

/// OpenAllocation associates its new private pointer with the global state under
/// an acquired dxgkrnl allocation reference. Binding resolves only the exact open
/// under its own reference; it never pairs two independently resolved handles.
pub struct Opens {
    entries: Vec<Option<Open>>,
}

impl Opens {
    pub fn new(capacity: usize) -> Result<Self, Error> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Capacity)?;
        entries.resize(capacity, None);
        Ok(Self { entries })
    }

    pub fn register(&mut self, pointer: usize, process: usize, key: Key) -> Result<(), Error> {
        if pointer == 0
            || key.generation == 0
            || self.entries.iter().flatten().any(|o| o.pointer == pointer)
        {
            return Err(Error::Invalid);
        }
        let entry = self
            .entries
            .iter_mut()
            .find(|o| o.is_none())
            .ok_or(Error::Capacity)?;
        *entry = Some(Open {
            pointer,
            process,
            key,
        });
        Ok(())
    }

    /// Caller holds the exact acquired runtime reference through this lookup
    /// and Table::retain, which rejects a removed or reused allocation generation.
    pub fn resolve(&self, pointer: usize, process: usize) -> Result<Key, Error> {
        if process == 0 {
            return Err(Error::Invalid);
        }
        let entry = self
            .entries
            .iter()
            .flatten()
            .find(|o| o.pointer == pointer && o.process == process)
            .ok_or(Error::Invalid)?;
        Ok(entry.key)
    }

    pub fn remove(&mut self, pointer: usize) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|o| o.is_some_and(|o| o.pointer == pointer))
        {
            *entry = None;
        }
    }
}

/// Scope one OS allocation reference around a status operation. A nonzero
/// release token owns a reference even if the requested private data is null.
/// Neither the private pointer nor a release token becomes a binding identity.
pub fn with_reference<T>(
    acquire: impl FnOnce() -> (usize, usize),
    release: impl FnOnce(usize),
    use_data: impl FnOnce(usize) -> Result<T, Error>,
) -> Result<T, Error> {
    struct Reference<F: FnOnce(usize)> {
        token: usize,
        release: Option<F>,
    }
    impl<F: FnOnce(usize)> Drop for Reference<F> {
        fn drop(&mut self) {
            if self.token != 0 {
                if let Some(release) = self.release.take() {
                    release(self.token);
                }
            }
        }
    }
    let (data, token) = acquire();
    let _reference = Reference {
        token,
        release: Some(release),
    };
    if data == 0 || token == 0 {
        return Err(Error::Invalid);
    }
    use_data(data)
}

#[derive(Clone, Copy)]
struct Allocation {
    id: usize,
    bindings: u32,
    dirty: bool,
    snapshot: Snapshot,
    head: usize,
    tail: usize,
}

impl Allocation {
    const EMPTY: Self = Self {
        id: 0,
        bindings: 0,
        dirty: false,
        snapshot: Snapshot {
            generation: 0,
            announced: 0,
            completed: 0,
            status: REMOVED,
        },
        head: NONE,
        tail: NONE,
    };
}

#[derive(Clone, Copy)]
struct Pending {
    key: Key,
    epoch: u64,
    stream: u64,
    value: u32,
    next: usize,
    done: bool,
    /// Index of the writer slot this entry counts against, or `NONE` (an entry
    /// that is already complete, or a `value == 0` entry, holds no writer).
    writer: usize,
}

/// Last published value of one (allocation, stream) pair, with the number of
/// entries of that pair still in the table. The slot is free again when the
/// last of them leaves.
#[derive(Clone, Copy)]
struct Writer {
    key: Key,
    stream: u64,
    value: u32,
    pending: u32,
}

/// Highest completed value of one live stream. Created by the stream's first
/// publication, freed with the stream. `watermark == 0`: nothing known complete.
#[derive(Clone, Copy)]
struct Mark {
    stream: u64,
    watermark: u32,
}

/// Default number of stream marks for [`Table::new`] (the present-stream table has 64).
pub const DEFAULT_MARKS: usize = 64;

/// Half of the u32 value space: the window inside which serial comparison is defined.
const HALF: u32 = 0x8000_0000;

/// `value` is at or before `completed` on the stream's timeline (serial-number
/// arithmetic, RFC 1982), so a completed `completed` proves `value`. Both are
/// nonzero stream values: `0` is "already complete" / "nothing completed" and
/// never a timeline position. Callers keep live values within half the space
/// of each other (the UMD retires a stream at `u32::MAX` and the host ring
/// holds far fewer than 2^31 values in flight), which is what makes the
/// comparison total and wrap-safe.
const fn reached(completed: u32, value: u32) -> bool {
    completed.wrapping_sub(value) < HALF
}

/// `a` is strictly later than `b` on the timeline.
const fn later(a: u32, b: u32) -> bool {
    a != b && a.wrapping_sub(b) < HALF
}

/// Occupancy and refusal counters, all monotonic except the three occupancy
/// gauges. Copyable so the caller can mirror them into atomics under its lock.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Pending entries in the table now (including done entries queued behind an older one).
    pub pending: u32,
    /// Highest `pending` since construction.
    pub pending_high: u32,
    /// Writer slots in use now.
    pub writers: u32,
    pub writers_high: u32,
    /// Stream marks in use now.
    pub marks: u32,
    /// Publications refused because no pending slot was free.
    pub refused_pending: u32,
    /// Publications refused because no writer slot was free.
    pub refused_writers: u32,
    /// Streams that could not get a mark (every mark slot taken): their early
    /// completions then rely on the caller's `already_complete` alone.
    pub marks_full: u32,
    /// Entries completed by a later value than their own (a skipped value).
    pub skipped: u32,
    /// Publications completed at once by the stream's remembered watermark.
    pub early: u32,
}

pub struct Table {
    allocations: Vec<Allocation>,
    pending: Vec<Option<Pending>>,
    writers: Vec<Option<Writer>>,
    marks: Vec<Option<Mark>>,
    generation: u64,
    dirty: Vec<u32>,
    stats: Stats,
}

/// Remove the entry at `i` and release what it held: its pending slot and its
/// share of a writer slot, which is freed with the last share. A free function
/// over the three fields so callers can hold other borrows of the table.
fn drop_entry(
    pending: &mut [Option<Pending>],
    writers: &mut [Option<Writer>],
    stats: &mut Stats,
    i: usize,
) {
    let Some(p) = pending[i].take() else {
        return;
    };
    stats.pending = stats.pending.saturating_sub(1);
    if let Some(slot) = writers.get_mut(p.writer) {
        if let Some(w) = slot {
            if w.key == p.key && w.stream == p.stream {
                w.pending = w.pending.saturating_sub(1);
                if w.pending == 0 {
                    *slot = None;
                    stats.writers = stats.writers.saturating_sub(1);
                }
            }
        }
    }
}

impl Table {
    /// Only construction allocates. No kernel stack-sized inline arrays.
    pub fn new(allocations: usize, pending: usize, writers: usize) -> Result<Self, Error> {
        Self::with_marks(allocations, pending, writers, DEFAULT_MARKS)
    }

    /// `marks` is the number of streams whose completed watermark is remembered
    /// at once (one slot per live stream; the present-stream table size).
    pub fn with_marks(
        allocations: usize,
        pending: usize,
        writers: usize,
        marks: usize,
    ) -> Result<Self, Error> {
        if allocations == 0 || allocations > u32::MAX as usize || pending == 0 || writers == 0 {
            return Err(Error::Invalid);
        }
        let mut a = Vec::new();
        let mut p = Vec::new();
        let mut w = Vec::new();
        let mut m = Vec::new();
        let mut dirty = Vec::new();
        a.try_reserve_exact(allocations)
            .map_err(|_| Error::Capacity)?;
        p.try_reserve_exact(pending).map_err(|_| Error::Capacity)?;
        w.try_reserve_exact(writers).map_err(|_| Error::Capacity)?;
        m.try_reserve_exact(marks).map_err(|_| Error::Capacity)?;
        dirty
            .try_reserve_exact(allocations)
            .map_err(|_| Error::Capacity)?;
        a.resize(allocations, Allocation::EMPTY);
        p.resize(pending, None);
        w.resize(writers, None);
        m.resize(marks, None);
        Ok(Self {
            allocations: a,
            pending: p,
            writers: w,
            marks: m,
            generation: 0,
            dirty,
            stats: Stats::default(),
        })
    }

    /// Occupancy gauges and refusal counters; a plain copy, valid under the caller's lock.
    pub fn stats(&self) -> Stats {
        self.stats
    }

    fn mark_index(&self, stream: u64) -> Option<usize> {
        self.marks
            .iter()
            .position(|m| m.is_some_and(|m| m.stream == stream))
    }

    fn note_pending_added(&mut self) {
        self.stats.pending += 1;
        self.stats.pending_high = self.stats.pending_high.max(self.stats.pending);
    }

    fn note_writer_added(&mut self) {
        self.stats.writers += 1;
        self.stats.writers_high = self.stats.writers_high.max(self.stats.writers);
    }

    /// Append `pending` (already stored) to the allocation's list and announce `epoch`.
    fn link(&mut self, key: Key, pending: usize, epoch: u64) {
        let a = &mut self.allocations[key.slot as usize];
        if let Some(tail) = self.pending.get_mut(a.tail).and_then(Option::as_mut) {
            tail.next = pending;
        } else {
            a.head = pending;
        }
        a.tail = pending;
        a.snapshot.announced = epoch;
        self.advance(key);
    }

    pub fn find(&self, id: usize) -> Option<Key> {
        if id == 0 {
            return None;
        }
        self.allocations
            .iter()
            .enumerate()
            .find(|(_, a)| a.id == id)
            .map(|(slot, a)| Key {
                slot: slot as u32,
                generation: a.snapshot.generation,
            })
    }

    pub fn register(&mut self, id: usize) -> Result<Key, Error> {
        if id == 0 || self.find(id).is_some() {
            return Err(Error::Invalid);
        }
        let slot = self
            .allocations
            .iter()
            .position(|a| a.id == 0 && a.bindings == 0)
            .ok_or(Error::Capacity)?;
        let generation = self.generation.checked_add(1).ok_or(Error::Capacity)?;
        self.generation = generation;
        self.allocations[slot] = Allocation {
            id,
            bindings: 0,
            dirty: self.allocations[slot].dirty,
            snapshot: Snapshot {
                generation,
                announced: 0,
                completed: 0,
                status: LIVE,
            },
            head: NONE,
            tail: NONE,
        };
        self.changed(slot);
        Ok(Key {
            slot: slot as u32,
            generation,
        })
    }

    pub fn snapshot(&self, key: Key) -> Result<Snapshot, Error> {
        let a = self
            .allocations
            .get(key.slot as usize)
            .ok_or(Error::Invalid)?;
        if key.generation == 0 || a.snapshot.generation != key.generation {
            return Err(Error::Invalid);
        }
        Ok(a.snapshot)
    }

    pub fn retain(&mut self, key: Key) -> Result<(), Error> {
        let s = self.snapshot(key)?;
        if s.status != LIVE {
            return Err(Error::Terminal(s.status));
        }
        let a = &mut self.allocations[key.slot as usize];
        a.bindings = a.bindings.checked_add(1).ok_or(Error::Capacity)?;
        Ok(())
    }

    pub fn release(&mut self, key: Key) -> Result<(), Error> {
        self.snapshot(key)?;
        let a = &mut self.allocations[key.slot as usize];
        a.bindings = a.bindings.checked_sub(1).ok_or(Error::Invalid)?;
        Ok(())
    }

    pub fn predicate(&self, key: Key, epoch: u64) -> Result<Predicate, Error> {
        let s = self.snapshot(key)?;
        if epoch > s.announced {
            return Err(Error::Invalid);
        }
        // Terminal state takes precedence even for an old completed target.
        if s.status != LIVE {
            return Ok(Predicate::Terminal(s.status));
        }
        Ok(if s.completed >= epoch {
            Predicate::Ready
        } else {
            Predicate::Pending
        })
    }

    /// `stream` is a generation-qualified registration, not its slot index.
    /// `already_complete` is supplied only by that exact stream's retirement
    /// proof under the same serialization as this call. Announcement may precede
    /// submission, and retirement may precede announcement.
    pub fn publish(
        &mut self,
        key: Key,
        stream: u64,
        value: u32,
        already_complete: bool,
    ) -> Result<u64, Error> {
        let s = self.snapshot(key)?;
        if s.status != LIVE {
            return Err(Error::Terminal(s.status));
        }
        if stream == 0 {
            return Err(Error::Invalid);
        }
        if value == 0 {
            return self.publish_complete(key, stream, s.announced);
        }
        let known = self
            .writers
            .iter()
            .position(|w| w.is_some_and(|w| w.key == key && w.stream == stream));
        // Strictly increasing per (allocation, stream) while an entry of the pair
        // is in flight. The slot is gone once its last entry retired, and a value
        // the stream has already completed is complete on arrival anyway.
        if known.is_some_and(|i| self.writers[i].is_some_and(|w| reached(w.value, value))) {
            return Err(Error::Invalid);
        }
        let mark = self.mark_index(stream);
        let remembered = mark
            .and_then(|i| self.marks[i])
            .is_some_and(|m| m.watermark != 0 && reached(m.watermark, value));
        let complete_now = already_complete || remembered;
        let epoch = s.announced.checked_add(1).ok_or(Error::Capacity)?;
        let alone = self.allocations[key.slot as usize].head == NONE;
        // All failure checks precede the first mutation: a rejected publication
        // never leaves an announced epoch with no dependency.
        let (pending, writer) = if complete_now && alone {
            // Nothing older is pending on this allocation and the stream is past
            // this value: announce and complete together, holding no slot.
            (NONE, NONE)
        } else {
            let pending = match self.pending.iter().position(Option::is_none) {
                Some(i) => i,
                None => {
                    self.stats.refused_pending = self.stats.refused_pending.saturating_add(1);
                    return Err(Error::Capacity);
                }
            };
            // A complete entry queued behind an older one needs no writer: it
            // names no in-flight position of the stream.
            let writer = if complete_now {
                NONE
            } else {
                match known.or_else(|| self.writers.iter().position(Option::is_none)) {
                    Some(i) => i,
                    None => {
                        self.stats.refused_writers = self.stats.refused_writers.saturating_add(1);
                        return Err(Error::Capacity);
                    }
                }
            };
            (pending, writer)
        };
        // Remember the stream's progress. `already_complete` is the caller's proof
        // that the stream retired `value`, so the watermark may be raised to it.
        match mark {
            Some(i) => {
                if let Some(m) = self.marks[i].as_mut() {
                    if already_complete && (m.watermark == 0 || later(value, m.watermark)) {
                        m.watermark = value;
                    }
                }
            }
            None => {
                if let Some(slot) = self.marks.iter_mut().find(|m| m.is_none()) {
                    *slot = Some(Mark {
                        stream,
                        watermark: if already_complete { value } else { 0 },
                    });
                    self.stats.marks += 1;
                } else {
                    self.stats.marks_full = self.stats.marks_full.saturating_add(1);
                }
            }
        }
        if remembered && !already_complete {
            self.stats.early = self.stats.early.saturating_add(1);
        }
        if pending == NONE {
            let a = &mut self.allocations[key.slot as usize];
            a.snapshot.announced = epoch;
            a.snapshot.completed = epoch;
            self.changed(key.slot as usize);
            return Ok(epoch);
        }
        if writer != NONE {
            let held = self.writers[writer].map_or(0, |w| w.pending);
            if held == 0 {
                self.note_writer_added();
            }
            self.writers[writer] = Some(Writer {
                key,
                stream,
                value,
                pending: held + 1,
            });
        }
        self.pending[pending] = Some(Pending {
            key,
            epoch,
            stream,
            value,
            next: NONE,
            done: complete_now,
            writer,
        });
        self.note_pending_added();
        self.link(key, pending, epoch);
        Ok(epoch)
    }

    /// `publish` with `value == 0`: the present is already complete (S3 of the
    /// DXVK-on-NVK plan: a CPU-complete producer waited for its own GPU work
    /// before announcing). There is no stream point to wait for, so:
    ///
    /// * the epoch is announced and completes at once when nothing older is
    ///   pending on this allocation, without consuming a `pending` slot;
    /// * behind an older pending epoch it queues as an already-done entry, so a
    ///   consumer still never sees epoch N+1 before epoch N (the per-allocation
    ///   prefix rule);
    /// * the stream's monotonic writer value is neither read nor advanced:
    ///   "complete" is not a position on the stream, so interleaving it with
    ///   ordinary values cannot trip the strictly-increasing rule or consume a
    ///   writer slot.
    ///
    /// Same all-checks-before-first-mutation rule as `publish`.
    fn publish_complete(&mut self, key: Key, stream: u64, announced: u64) -> Result<u64, Error> {
        let epoch = announced.checked_add(1).ok_or(Error::Capacity)?;
        let a = &self.allocations[key.slot as usize];
        if a.head == NONE {
            let a = &mut self.allocations[key.slot as usize];
            a.snapshot.announced = epoch;
            a.snapshot.completed = epoch;
            self.changed(key.slot as usize);
            return Ok(epoch);
        }
        let Some(pending) = self.pending.iter().position(Option::is_none) else {
            self.stats.refused_pending = self.stats.refused_pending.saturating_add(1);
            return Err(Error::Capacity);
        };
        self.pending[pending] = Some(Pending {
            key,
            epoch,
            stream,
            value: 0,
            next: NONE,
            done: true,
            writer: NONE,
        });
        self.note_pending_added();
        self.link(key, pending, epoch);
        Ok(epoch)
    }

    fn advance(&mut self, key: Key) {
        let slot = key.slot as usize;
        let mut head = self.allocations[slot].head;
        let mut completed = None;
        while let Some(p) = self.pending.get(head).copied().flatten() {
            if !p.done {
                break;
            }
            completed = Some(p.epoch);
            drop_entry(&mut self.pending, &mut self.writers, &mut self.stats, head);
            head = p.next;
        }
        let a = &mut self.allocations[slot];
        if let Some(epoch) = completed {
            a.snapshot.completed = epoch;
        }
        a.head = head;
        if head == NONE {
            a.tail = NONE;
        }
        self.changed(slot);
    }

    /// The stream retired through `value` (its cumulative completed value, not
    /// necessarily the submission that just returned). A timeline value proves
    /// every earlier value of the same stream, so every pending entry of `stream`
    /// at or before it completes, whatever order or batching delivered it. The
    /// per-allocation linked prefix still decides when an epoch becomes visible.
    ///
    /// `value == 0` is "nothing retired yet" and completes nothing. A stream
    /// this table has not published on is ignored: the first publication brings
    /// the caller's `already_complete` proof.
    ///
    /// No allocation; one pass over the pending slots.
    pub fn complete(&mut self, stream: u64, value: u32) {
        if value == 0 || stream == 0 {
            return;
        }
        let mut through = value;
        if let Some(m) = self.mark_index(stream).and_then(|i| self.marks[i].as_mut()) {
            if m.watermark == 0 || later(value, m.watermark) {
                m.watermark = value;
            } else {
                // A late, smaller report: the watermark never regresses.
                through = m.watermark;
            }
        }
        if self.stats.pending == 0 {
            return;
        }
        for i in 0..self.pending.len() {
            let Some(p) = self.pending[i] else {
                continue;
            };
            if p.stream != stream || p.done || p.value == 0 || !reached(through, p.value) {
                continue;
            }
            if p.value != value {
                self.stats.skipped = self.stats.skipped.saturating_add(1);
            }
            if let Some(entry) = &mut self.pending[i] {
                entry.done = true;
            }
            self.advance(p.key);
        }
    }

    pub fn fail_stream(&mut self, stream: u64, status: u32) {
        if status == LIVE {
            return;
        }
        // Mark affected allocations first, then sweep pending entries once.
        // Reset/stream teardown must stay linear while the kernel lock is held.
        for i in 0..self.pending.len() {
            if let Some(p) = self.pending[i] {
                if p.stream == stream {
                    let a = &mut self.allocations[p.key.slot as usize];
                    if a.snapshot.status == LIVE {
                        a.snapshot.status = status;
                    }
                    a.head = NONE;
                    a.tail = NONE;
                    self.changed(p.key.slot as usize);
                }
            }
        }
        for i in 0..self.pending.len() {
            if self.pending[i]
                .is_some_and(|p| self.allocations[p.key.slot as usize].snapshot.status != LIVE)
            {
                drop_entry(&mut self.pending, &mut self.writers, &mut self.stats, i);
            }
        }
        // Every entry of the stream is gone, so its writers and mark are too.
        for w in &mut self.writers {
            if w.is_some_and(|w| w.stream == stream) {
                *w = None;
                self.stats.writers = self.stats.writers.saturating_sub(1);
            }
        }
        for m in &mut self.marks {
            if m.is_some_and(|m| m.stream == stream) {
                *m = None;
                self.stats.marks = self.stats.marks.saturating_sub(1);
            }
        }
    }

    pub fn terminal(&mut self, key: Key, status: u32) {
        if self.snapshot(key).is_err() || status == LIVE {
            return;
        }
        let a = &mut self.allocations[key.slot as usize];
        if a.snapshot.status == LIVE {
            a.snapshot.status = status;
        }
        // Failure/cancellation never manufactures a completion.
        a.head = NONE;
        a.tail = NONE;
        for i in 0..self.pending.len() {
            if self.pending[i].is_some_and(|p| p.key == key) {
                drop_entry(&mut self.pending, &mut self.writers, &mut self.stats, i);
            }
        }
        self.changed(key.slot as usize);
    }

    /// Called only when dxgkrnl destroys the allocation, after every acquired
    /// reference has been released. The mapped storage itself remains alive.
    pub fn remove(&mut self, key: Key) {
        if self.snapshot(key).is_err() {
            return;
        }
        self.terminal(key, REMOVED);
        self.allocations[key.slot as usize].id = 0;
        for w in &mut self.writers {
            if w.is_some_and(|w| w.key == key) {
                *w = None;
                self.stats.writers = self.stats.writers.saturating_sub(1);
            }
        }
    }

    pub fn reset(&mut self) {
        for i in 0..self.allocations.len() {
            let a = &mut self.allocations[i];
            if a.id != 0 {
                if a.snapshot.status == LIVE {
                    a.snapshot.status = REMOVED;
                }
                a.head = NONE;
                a.tail = NONE;
                self.changed(i);
            }
        }
        self.pending.fill(None);
        self.writers.fill(None);
        self.marks.fill(None);
        self.stats.pending = 0;
        self.stats.writers = 0;
        self.stats.marks = 0;
    }

    pub fn slots(&self) -> usize {
        self.allocations.len()
    }
    pub fn slot_snapshot(&self, slot: usize) -> Option<Snapshot> {
        self.allocations.get(slot).map(|a| a.snapshot)
    }

    fn changed(&mut self, slot: usize) {
        if !self.allocations[slot].dirty {
            self.allocations[slot].dirty = true;
            self.dirty.push(slot as u32);
        }
    }

    pub fn take_changed(&mut self) -> Option<(u32, Snapshot)> {
        let slot = self.dirty.pop()?;
        let a = &mut self.allocations[slot as usize];
        a.dirty = false;
        Some((slot, a.snapshot))
    }
}

/// Referenced-event ownership without OS operations. The caller holds the same
/// lock for Table changes, registration and draining; the returned event is
/// transferred exactly once, either to cancellation or to the wake callback.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wait {
    pub binding: u64,
    pub owner: usize,
    pub key: Key,
    pub epoch: u64,
    pub event: usize,
}

pub struct Waiters {
    entries: Vec<Option<Wait>>,
}

impl Waiters {
    pub fn new(capacity: usize) -> Result<Self, Error> {
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| Error::Capacity)?;
        entries.resize(capacity, None);
        Ok(Self { entries })
    }

    pub fn register(&mut self, table: &Table, wait: Wait) -> Result<Predicate, Error> {
        if wait.event == 0 || wait.binding == 0 || wait.owner == 0 {
            return Err(Error::Invalid);
        }
        let predicate = table.predicate(wait.key, wait.epoch)?;
        if predicate == Predicate::Pending {
            if self
                .entries
                .iter()
                .flatten()
                .any(|w| w.owner == wait.owner && w.event == wait.event)
            {
                return Err(Error::Invalid);
            }
            let slot = self
                .entries
                .iter_mut()
                .find(|w| w.is_none())
                .ok_or(Error::Capacity)?;
            *slot = Some(wait);
        }
        Ok(predicate)
    }

    pub fn cancel(&mut self, owner: usize, binding: u64, event: usize) -> Option<Wait> {
        self.entries
            .iter_mut()
            .find(|w| {
                w.is_some_and(|w| w.owner == owner && w.binding == binding && w.event == event)
            })?
            .take()
    }

    pub fn drain(
        &mut self,
        table: &Table,
        owner: Option<usize>,
        binding: Option<u64>,
        mut wake: impl FnMut(Wait),
    ) {
        for slot in &mut self.entries {
            let Some(wait) = *slot else {
                continue;
            };
            let cancelled =
                owner == Some(wait.owner) && (binding.is_none() || binding == Some(wait.binding));
            if cancelled
                || !matches!(
                    table.predicate(wait.key, wait.epoch),
                    Ok(Predicate::Pending)
                )
            {
                *slot = None;
                wake(wait);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn table() -> Table {
        Table::new(4, 8, 8).unwrap()
    }
    fn wait(key: Key, epoch: u64) -> Wait {
        Wait {
            owner: 1,
            binding: 1,
            key,
            epoch,
            event: 1,
        }
    }

    #[test]
    fn announcement_before_submission_stays_pending() {
        let mut t = table();
        let a = t.register(11).unwrap();
        assert_eq!(t.publish(a, 101, 77, false), Ok(1));
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Pending));
        t.complete(102, 77);
        t.complete(101, 76); // wrong stream/value
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Pending));
        t.complete(101, 77);
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Ready));
    }

    #[test]
    fn exact_retirement_before_publication_is_ready() {
        let mut t = table();
        let a = t.register(11).unwrap();
        // Production passes the exact registered stream's retirement proof.
        assert_eq!(t.publish(a, 101, 77, true), Ok(1));
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Ready));
    }

    #[test]
    fn out_of_order_resources_advance_independently_and_only_through_prefix() {
        let mut t = table();
        let a = t.register(11).unwrap();
        let b = t.register(12).unwrap();
        t.publish(a, 101, 40, false).unwrap();
        t.publish(a, 202, 1, false).unwrap(); // independent producer's namespace
        t.publish(b, 202, 1, false).unwrap();
        t.complete(202, 1);
        assert_eq!(t.snapshot(a).unwrap().completed, 0);
        assert_eq!(t.snapshot(b).unwrap().completed, 1);
        t.complete(101, 40);
        assert_eq!(t.snapshot(a).unwrap().completed, 2);
    }

    #[test]
    fn completed_later_epoch_does_not_cover_pending_earlier_epoch_on_publication() {
        let mut t = table();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, false).unwrap();
        t.publish(a, 202, 6, true).unwrap();
        assert_eq!(t.predicate(a, 2), Ok(Predicate::Pending));
        t.complete(101, 1);
        assert_eq!(t.predicate(a, 2), Ok(Predicate::Ready));
    }

    #[test]
    fn independent_opens_retain_one_allocation_incarnation() {
        let mut t = table();
        let a = t.register(11).unwrap();
        let open1 = t.find(11).unwrap();
        let open2 = t.find(11).unwrap();
        t.retain(open1).unwrap();
        t.retain(open2).unwrap();
        t.publish(open1, 101, 1, false).unwrap();
        assert_eq!(t.snapshot(open2).unwrap().announced, 1);
        t.release(open1).unwrap(); // consumer release is not producer completion
        assert_eq!(t.predicate(open2, 1), Ok(Predicate::Pending));
        t.complete(101, 1);
        assert_eq!(t.snapshot(open2), t.snapshot(a));
    }

    #[test]
    fn stale_generation_and_recycled_pointer_never_inherit_readiness() {
        let mut t = Table::new(1, 4, 4).unwrap();
        let a = t.register(11).unwrap();
        t.retain(a).unwrap();
        t.publish(a, 101, 1, false).unwrap();
        t.remove(a);
        assert_eq!(t.register(11), Err(Error::Capacity)); // mapped binding keeps slot alive
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Terminal(REMOVED)));
        t.release(a).unwrap();
        let b = t.register(11).unwrap();
        assert_ne!(a.generation, b.generation);
        assert_eq!(t.snapshot(a), Err(Error::Invalid));
        t.publish(b, 202, 1, false).unwrap();
        t.complete(101, 1);
        assert_eq!(t.predicate(b, 1), Ok(Predicate::Pending));
    }

    #[test]
    fn rejection_is_atomic_and_stream_values_are_not_resource_epochs() {
        let mut t = Table::new(2, 1, 2).unwrap();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 900, false).unwrap();
        let before = t.snapshot(a).unwrap();
        for (stream, value, error) in [
            (101, 900, Error::Invalid),
            (101, 899, Error::Invalid),
            (0, 1, Error::Invalid),
            (0, 0, Error::Invalid), // a complete publish still names a stream
            (202, 1, Error::Capacity),
        ] {
            assert_eq!(t.publish(a, stream, value, false), Err(error));
            assert_eq!(t.snapshot(a).unwrap(), before);
        }
        t.complete(101, 900);
        assert_eq!(t.publish(a, 202, 1, false), Ok(2));
    }

    #[test]
    fn value_zero_is_already_complete_without_a_pending_slot() {
        // One pending slot, and it is used: a complete publish must not need it.
        let mut t = Table::new(2, 1, 2).unwrap();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 5, false).unwrap();
        t.complete(101, 5);
        assert_eq!(t.snapshot(a).unwrap().completed, 1);
        // Nothing pending: epoch 2 announces and completes together.
        assert_eq!(t.publish(a, 101, 0, false), Ok(2));
        let s = t.snapshot(a).unwrap();
        assert_eq!((s.announced, s.completed), (2, 2));
        assert_eq!(t.predicate(a, 2), Ok(Predicate::Ready));
        // And the stream's own value namespace is untouched: 5 is already
        // complete on stream 101 (its writer slot left with its last entry), so a
        // replay of it completes on arrival instead of queuing; 6 is next.
        assert_eq!(t.publish(a, 101, 5, false), Ok(3));
        assert_eq!(t.predicate(a, 3), Ok(Predicate::Ready));
        assert_eq!(t.publish(a, 101, 6, false), Ok(4));
        assert_eq!(t.predicate(a, 4), Ok(Predicate::Pending));
    }

    #[test]
    fn value_zero_never_overtakes_an_older_pending_epoch() {
        let mut t = table();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, false).unwrap(); // epoch 1, pending
        assert_eq!(t.publish(a, 202, 0, false), Ok(2)); // epoch 2, complete
                                                        // The consumer waiting on 2 must not run ahead of epoch 1.
        assert_eq!(t.predicate(a, 2), Ok(Predicate::Pending));
        assert_eq!(t.snapshot(a).unwrap().completed, 0);
        t.complete(101, 1);
        assert_eq!(t.snapshot(a).unwrap().completed, 2);
        assert_eq!(t.predicate(a, 2), Ok(Predicate::Ready));
    }

    #[test]
    fn value_zero_repeats_freely_and_takes_no_writer_slot() {
        // Two writer slots, both free; a hundred complete publishes need none.
        let mut t = Table::new(1, 2, 1).unwrap();
        let a = t.register(11).unwrap();
        for epoch in 1..=100u64 {
            assert_eq!(t.publish(a, 101, 0, false), Ok(epoch));
        }
        assert_eq!(t.snapshot(a).unwrap().completed, 100);
        // The single writer slot is still free for a real stream.
        assert_eq!(t.publish(a, 303, 1, false), Ok(101));
    }

    #[test]
    fn value_zero_behind_pending_needs_a_pending_slot_and_fails_atomically() {
        let mut t = Table::new(1, 1, 2).unwrap();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, false).unwrap(); // takes the only pending slot
        let before = t.snapshot(a).unwrap();
        assert_eq!(t.publish(a, 101, 0, false), Err(Error::Capacity));
        assert_eq!(t.snapshot(a).unwrap(), before);
    }

    #[test]
    fn value_zero_on_a_terminal_allocation_is_refused() {
        let mut t = table();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, false).unwrap();
        t.fail_stream(101, FAILED);
        assert_eq!(t.publish(a, 101, 0, false), Err(Error::Terminal(FAILED)));
        let b = t.register(12).unwrap();
        t.remove(b);
        assert_eq!(t.publish(b, 101, 0, false), Err(Error::Terminal(REMOVED)));
    }

    #[test]
    fn a_failed_stream_does_not_cancel_a_complete_epoch_that_is_already_done() {
        let mut t = table();
        let a = t.register(11).unwrap();
        assert_eq!(t.publish(a, 101, 0, false), Ok(1));
        t.fail_stream(101, FAILED);
        // No pending entry named the stream, so the allocation is not poisoned
        // and the epoch stays complete.
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Ready));
    }

    #[test]
    fn producer_failure_cancels_prefix_without_completing_it() {
        let mut t = table();
        let a = t.register(11).unwrap();
        let b = t.register(12).unwrap();
        t.publish(a, 101, 1, true).unwrap();
        t.publish(a, 101, 2, false).unwrap();
        t.publish(a, 202, 5, true).unwrap();
        t.publish(b, 202, 5, true).unwrap();
        t.fail_stream(101, FAILED);
        t.complete(101, 2);
        assert_eq!(t.snapshot(a).unwrap().completed, 1);
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Terminal(FAILED)));
        assert_eq!(t.publish(a, 303, 1, true), Err(Error::Terminal(FAILED)));
        assert_eq!(t.predicate(b, 1), Ok(Predicate::Ready));
    }

    #[test]
    fn register_retire_and_cancel_permutations_transfer_event_once() {
        for retire_first in [false, true] {
            for cancel_first in [false, true] {
                let mut t = table();
                let mut w = Waiters::new(2).unwrap();
                let a = t.register(11).unwrap();
                t.publish(a, 101, 1, false).unwrap();
                let request = wait(a, 1);
                if retire_first {
                    t.complete(101, 1);
                }
                let predicate = w.register(&t, request).unwrap();
                let mut transferred = usize::from(predicate != Predicate::Pending);
                if cancel_first {
                    transferred += usize::from(w.cancel(1, 1, 1).is_some());
                }
                t.complete(101, 1);
                w.drain(&t, None, None, |_| transferred += 1);
                transferred += usize::from(w.cancel(1, 1, 1).is_some());
                w.drain(&t, None, None, |_| panic!("event transferred twice"));
                assert_eq!(transferred, 1);
            }
        }
    }

    #[test]
    fn cancelled_event_reuse_cannot_cancel_a_different_binding() {
        let mut t = table();
        let mut w = Waiters::new(2).unwrap();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, false).unwrap();
        let request = wait(a, 1);
        w.register(&t, request).unwrap();
        assert_eq!(w.cancel(2, 1, 1), None);
        assert_eq!(w.cancel(1, 2, 1), None);
        assert_eq!(w.cancel(1, 1, 1), Some(request));
        let next = Wait {
            binding: 2,
            ..request
        };
        w.register(&t, next).unwrap();
        assert_eq!(w.cancel(1, 1, 1), None);
        t.complete(101, 1);
        let mut got = None;
        w.drain(&t, None, None, |x| got = Some(x));
        assert_eq!(got, Some(next));
    }

    #[test]
    fn binding_close_only_cancels_its_own_waiters() {
        let mut t = table();
        let mut w = Waiters::new(3).unwrap();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, false).unwrap();
        for binding in 1..=3 {
            w.register(
                &t,
                Wait {
                    binding,
                    event: binding as usize,
                    ..wait(a, 1)
                },
            )
            .unwrap();
        }
        let mut got = Vec::new();
        w.drain(&t, Some(1), Some(2), |x| got.push(x.binding));
        assert_eq!(got, [2]);
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Pending));
        w.drain(&t, Some(1), None, |x| got.push(x.binding));
        assert_eq!(got, [2, 1, 3]);
    }

    #[test]
    fn reset_and_allocation_teardown_wake_pending_without_completion() {
        for reset in [false, true] {
            let mut t = table();
            let mut w = Waiters::new(1).unwrap();
            let a = t.register(11).unwrap();
            t.publish(a, 101, 1, false).unwrap();
            w.register(&t, wait(a, 1)).unwrap();
            if reset {
                t.reset();
            } else {
                t.remove(a);
            }
            let mut count = 0;
            w.drain(&t, None, None, |_| count += 1);
            assert_eq!(count, 1);
            assert_eq!(t.snapshot(a).unwrap().completed, 0);
            assert_eq!(t.predicate(a, 1), Ok(Predicate::Terminal(REMOVED)));
            assert_eq!(w.register(&t, wait(a, 1)), Ok(Predicate::Terminal(REMOVED)));
        }
    }

    #[test]
    fn invalid_future_or_capacity_wait_does_not_take_event_ownership() {
        let mut t = table();
        let mut w = Waiters::new(1).unwrap();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, false).unwrap();
        assert_eq!(w.register(&t, wait(a, 2)), Err(Error::Invalid));
        w.register(&t, wait(a, 1)).unwrap();
        assert_eq!(w.register(&t, wait(a, 1)), Err(Error::Invalid));
        assert_eq!(
            w.register(
                &t,
                Wait {
                    event: 2,
                    ..wait(a, 1)
                }
            ),
            Err(Error::Capacity)
        );
        assert_eq!(w.cancel(1, 1, 2), None);
    }

    #[test]
    fn dirty_view_reports_latest_snapshot_once_after_slot_reuse() {
        let mut t = table();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, true).unwrap();
        t.remove(a);
        let b = t.register(12).unwrap();
        assert_eq!(a.slot, b.slot);
        assert_eq!(t.take_changed(), Some((b.slot, t.snapshot(b).unwrap())));
        assert_eq!(t.take_changed(), None);
    }

    #[test]
    fn different_process_opens_share_the_registered_global_allocation() {
        let mut t = table();
        let mut opens = Opens::new(3).unwrap();
        let key = t.register(11).unwrap();
        opens.register(101, 1, key).unwrap();
        opens.register(102, 2, key).unwrap();
        t.retain(opens.resolve(101, 1).unwrap()).unwrap();
        t.retain(opens.resolve(102, 2).unwrap()).unwrap();
        t.publish(key, 1001, 1, true).unwrap();
        assert_eq!(t.snapshot(key).unwrap().completed, 1);
        opens.remove(101);
        t.release(key).unwrap();
        assert_eq!(opens.resolve(102, 2), Ok(key));
        assert_eq!(t.snapshot(key).unwrap().completed, 1);
    }

    #[test]
    fn open_resolution_rejects_foreign_process_and_changed_generation() {
        let mut t = table();
        let mut opens = Opens::new(1).unwrap();
        let old = t.register(11).unwrap();
        opens.register(101, 1, old).unwrap();
        assert_eq!(opens.resolve(101, 2), Err(Error::Invalid));
        assert_eq!(opens.resolve(999, 1), Err(Error::Invalid));
        assert_eq!(opens.resolve(101, 1), Ok(old));
        t.remove(old);
        let new = t.register(12).unwrap();
        assert_eq!(old.slot, new.slot);
        // A status slot can be reused, but the open cannot retarget itself.
        assert_eq!(
            t.retain(opens.resolve(101, 1).unwrap()),
            Err(Error::Invalid)
        );
        opens.remove(101);
        assert_eq!(opens.resolve(101, 1), Err(Error::Invalid));
        opens.register(101, 1, new).unwrap();
        assert_eq!(opens.resolve(101, 1), Ok(new));
    }

    #[test]
    fn opens_release_capacity_on_close_and_require_a_process_for_binding() {
        let key = Key {
            slot: 0,
            generation: 1,
        };
        let mut opens = Opens::new(1).unwrap();
        opens.register(101, 0, key).unwrap();
        assert_eq!(opens.resolve(101, 0), Err(Error::Invalid));
        assert_eq!(opens.register(101, 1, key), Err(Error::Invalid));
        assert_eq!(opens.register(102, 1, key), Err(Error::Capacity));
        opens.remove(101);
        opens.register(102, 1, key).unwrap();
    }

    #[test]
    fn acquired_reference_spans_use_and_releases_once_on_success_or_refusal() {
        use core::cell::Cell;
        for result in [Ok(7), Err(Error::Capacity), Err(Error::Terminal(REMOVED))] {
            let held = Cell::new(false);
            let released = Cell::new(0);
            let got = with_reference(
                || {
                    held.set(true);
                    (11, 12)
                },
                |token| {
                    assert_eq!(token, 12);
                    assert!(held.replace(false));
                    released.set(released.get() + 1);
                },
                |data| {
                    assert_eq!(data, 11);
                    assert!(held.get());
                    result
                },
            );
            assert_eq!(got, result);
            assert!(!held.get());
            assert_eq!(released.get(), 1);
        }
    }

    #[test]
    fn null_private_data_still_releases_an_acquired_reference() {
        use core::cell::Cell;
        let released = Cell::new(0);
        let result: Result<(), Error> = with_reference(
            || (0, 12),
            |token| {
                assert_eq!(token, 12);
                released.set(released.get() + 1);
            },
            |_| panic!("null private data must not be used"),
        );
        assert_eq!(result, Err(Error::Invalid));
        assert_eq!(released.get(), 1);
    }

    #[test]
    fn failed_acquire_does_not_release_or_use_unprotected_data() {
        for data in [0, 11] {
            let result: Result<(), Error> = with_reference(
                || (data, 0),
                |_| panic!("no reference was acquired"),
                |_| panic!("unprotected private data must not be used"),
            );
            assert_eq!(result, Err(Error::Invalid));
        }
    }

    // ---- completion rule, early completion, slot lifetime, occupancy ----

    fn big() -> Table {
        Table::new(8, 64, 64).unwrap()
    }

    #[test]
    fn exact_value_completes_exactly_that_entry() {
        let mut t = big();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 5, false).unwrap();
        t.complete(101, 5);
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Ready));
        assert_eq!(t.stats().pending, 0);
        assert_eq!(t.stats().skipped, 0);
    }

    #[test]
    fn a_skipped_value_is_covered_by_a_later_completion() {
        // Values 1..=3 are pending on three allocations; the host reports only 3
        // (batched, or 1 and 2 were never tagged): all three complete.
        let mut t = big();
        let keys: Vec<Key> = (0..3).map(|i| t.register(11 + i).unwrap()).collect();
        for (i, k) in keys.iter().enumerate() {
            t.publish(*k, 101, i as u32 + 1, false).unwrap();
        }
        t.complete(101, 3);
        for k in &keys {
            assert_eq!(t.predicate(*k, 1), Ok(Predicate::Ready));
        }
        assert_eq!(t.stats().pending, 0);
        assert_eq!(t.stats().writers, 0);
        assert_eq!(t.stats().skipped, 2);
    }

    #[test]
    fn a_skipped_head_no_longer_holds_its_allocations_later_entries() {
        // The defect: entry 1 never gets its own report, 2 and 3 do. With the
        // exact rule 2 and 3 sat `done` behind 1 forever, one slot each.
        let mut t = big();
        let a = t.register(11).unwrap();
        for v in 1..=3 {
            t.publish(a, 101, v, false).unwrap();
        }
        t.complete(101, 2);
        assert_eq!(t.snapshot(a).unwrap().completed, 2);
        t.complete(101, 3);
        assert_eq!(t.snapshot(a).unwrap().completed, 3);
        assert_eq!(t.stats().pending, 0);
    }

    #[test]
    fn out_of_order_reports_never_regress_or_double_apply() {
        let mut t = big();
        let a = t.register(11).unwrap();
        for v in [9, 10, 12] {
            t.publish(a, 101, v, false).unwrap();
        }
        t.complete(101, 12); // 12 retires first: the watermark covers 9 and 10
        assert_eq!(t.snapshot(a).unwrap().completed, 3);
        t.complete(101, 9); // the late smaller report completes nothing new
        assert_eq!(t.snapshot(a).unwrap().completed, 3);
        let epoch = t.publish(a, 101, 13, false).unwrap();
        t.complete(101, 9); // and never lowers the watermark below 12
        assert_eq!(t.predicate(a, epoch), Ok(Predicate::Pending));
        // A replay below the pair's last value is refused while 13 is in flight;
        // another allocation's 11 is behind the watermark and complete on arrival.
        assert_eq!(t.publish(a, 101, 11, false), Err(Error::Invalid));
        let b = t.register(12).unwrap();
        assert_eq!(t.publish(b, 101, 11, false), Ok(1));
        assert_eq!(t.predicate(b, 1), Ok(Predicate::Ready));
        t.complete(101, 13);
        assert_eq!(t.predicate(a, epoch), Ok(Predicate::Ready));
    }

    #[test]
    fn a_completion_does_not_cover_another_stream_or_a_later_value() {
        let mut t = big();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 5, false).unwrap();
        t.publish(a, 202, 5, false).unwrap();
        t.complete(101, 4);
        assert_eq!(t.snapshot(a).unwrap().completed, 0);
        t.complete(101, 5);
        assert_eq!(t.snapshot(a).unwrap().completed, 1);
        t.complete(0, 9);
        t.complete(101, 0);
        assert_eq!(t.snapshot(a).unwrap().completed, 1);
        t.complete(202, 9);
        assert_eq!(t.snapshot(a).unwrap().completed, 2);
    }

    #[test]
    fn completion_before_publication_completes_on_arrival() {
        // The caller's proof is absent (false) but the table has seen the
        // stream's retirement: first publication establishes the stream, then a
        // report, then publications behind it.
        let mut t = big();
        let a = t.register(11).unwrap();
        let b = t.register(12).unwrap();
        t.publish(a, 101, 1, false).unwrap();
        t.complete(101, 7);
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Ready));
        // 5 <= 7 and 7 itself arrive after their retirement.
        for v in [5, 7] {
            let epoch = t.publish(b, 101, v, false).unwrap();
            assert_eq!(t.predicate(b, epoch), Ok(Predicate::Ready));
        }
        assert_eq!(t.stats().early, 2);
        assert_eq!(t.stats().pending, 0);
        // 8 is beyond the watermark and waits.
        let epoch = t.publish(b, 101, 8, false).unwrap();
        assert_eq!(t.predicate(b, epoch), Ok(Predicate::Pending));
        t.complete(101, 8);
        assert_eq!(t.predicate(b, epoch), Ok(Predicate::Ready));
    }

    #[test]
    fn early_completion_queues_behind_an_older_pending_epoch() {
        let mut t = big();
        let a = t.register(11).unwrap();
        t.publish(a, 202, 1, false).unwrap(); // epoch 1, stream 202 never reports
        t.publish(a, 101, 3, false).unwrap(); // epoch 2, establishes stream 101
        t.complete(101, 9);
        let epoch = t.publish(a, 101, 4, false).unwrap();
        // Complete on arrival, but the prefix rule keeps it behind epoch 1.
        assert_eq!(epoch, 3);
        assert_eq!(t.predicate(a, 3), Ok(Predicate::Pending));
        assert_eq!(t.snapshot(a).unwrap().completed, 0);
        t.complete(202, 1);
        assert_eq!(t.snapshot(a).unwrap().completed, 3);
        assert_eq!(t.stats().pending, 0);
        assert_eq!(t.stats().writers, 0);
    }

    #[test]
    fn caller_proof_seeds_the_watermark() {
        let mut t = big();
        let a = t.register(11).unwrap();
        assert_eq!(t.publish(a, 101, 6, true), Ok(1));
        // Nothing else told the table about stream 101; the proof for 6 did.
        let epoch = t.publish(a, 101, 4, false).unwrap();
        assert_eq!(t.predicate(a, epoch), Ok(Predicate::Ready));
        let epoch = t.publish(a, 101, 7, false).unwrap();
        assert_eq!(t.predicate(a, epoch), Ok(Predicate::Pending));
    }

    #[test]
    fn writer_slots_are_reused_after_the_last_entry_retires() {
        // One writer slot, many (allocation, stream) pairs in turn.
        let mut t = Table::new(4, 8, 1).unwrap();
        let keys: Vec<Key> = (0..4).map(|i| t.register(11 + i).unwrap()).collect();
        for round in 0..50u32 {
            let k = keys[round as usize % 4];
            let stream = 100 + u64::from(round);
            t.publish(k, stream, 1, false).unwrap();
            assert_eq!(t.stats().writers, 1);
            t.complete(stream, 1);
            assert_eq!(t.stats().writers, 0);
        }
        assert_eq!(t.stats().writers_high, 1);
        assert_eq!(t.stats().refused_writers, 0);
    }

    #[test]
    fn a_writer_slot_stays_while_any_entry_of_its_pair_is_pending() {
        let mut t = Table::new(2, 8, 2).unwrap();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, false).unwrap();
        t.publish(a, 101, 2, false).unwrap();
        t.complete(101, 1);
        assert_eq!(t.stats().writers, 1);
        // Still in flight: the monotonic rule holds.
        assert_eq!(t.publish(a, 101, 2, false), Err(Error::Invalid));
        t.complete(101, 2);
        assert_eq!(t.stats().writers, 0);
    }

    #[test]
    fn writer_exhaustion_is_counted_and_clears_on_retirement() {
        let mut t = Table::new(4, 8, 2).unwrap();
        let k: Vec<Key> = (0..3).map(|i| t.register(11 + i).unwrap()).collect();
        t.publish(k[0], 101, 1, false).unwrap();
        t.publish(k[1], 101, 1, false).unwrap();
        assert_eq!(t.publish(k[2], 101, 1, false), Err(Error::Capacity));
        assert_eq!(t.stats().refused_writers, 1);
        assert_eq!(t.snapshot(k[2]).unwrap().announced, 0);
        t.complete(101, 1);
        assert_eq!(t.publish(k[2], 101, 2, false), Ok(1));
    }

    #[test]
    fn pending_exhaustion_is_counted_and_a_completion_recovers_it() {
        let mut t = Table::new(1, 3, 3).unwrap();
        let a = t.register(11).unwrap();
        for v in 1..=3 {
            t.publish(a, 101, v, false).unwrap();
        }
        assert_eq!(t.publish(a, 101, 4, false), Err(Error::Capacity));
        assert_eq!(t.stats().refused_pending, 1);
        assert_eq!(t.stats().pending_high, 3);
        t.complete(101, 3);
        assert_eq!(t.publish(a, 101, 4, false), Ok(4));
    }

    #[test]
    fn the_exact_rule_would_have_filled_the_table_and_this_one_does_not() {
        // Reproduces the field failure shape: one allocation, a value that never
        // gets its own report, then a long run of exactly reported presents.
        const PENDING: usize = 32;
        let mut t = Table::new(1, PENDING, 4).unwrap();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, false).unwrap(); // never reported on its own
        for v in 2..=10_000u32 {
            t.publish(a, 101, v, false).unwrap();
            t.complete(101, v);
            assert!(t.stats().pending <= 2, "{:?}", t.stats());
        }
        assert_eq!(t.stats().pending, 0);
        assert_eq!(t.snapshot(a).unwrap().completed, 10_000);
    }

    #[test]
    fn soak_never_exceeds_a_bound_with_skipped_and_early_reports() {
        // 100k presents over 3 allocations and 2 streams. Reports are batched
        // (some values never reported on their own), repeated late, and
        // sometimes arrive before the publication they cover. Every stream
        // reports its latest value every 8 presents, which bounds what can be
        // outstanding. The table must never fill.
        const PENDING: usize = 64;
        let mut t = Table::new(4, PENDING, 16).unwrap();
        let keys: Vec<Key> = (0..3).map(|i| t.register(11 + i).unwrap()).collect();
        let mut value = [0u32; 2];
        let mut reported = [0u32; 2];
        let mut seed = 0x1234_5678u32;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for i in 0..100_000u32 {
            let s = (next() % 2) as usize;
            let stream = 101 + s as u64;
            value[s] += 1;
            let k = keys[(i % 3) as usize];
            if next() % 7 == 0 {
                // The report beats the publication it covers.
                reported[s] = value[s];
                t.complete(stream, value[s]);
            }
            let r = t.publish(k, stream, value[s], false);
            assert!(r.is_ok(), "publish {i} refused: {r:?} {:?}", t.stats());
            match next() % 5 {
                0 | 1 => {
                    reported[s] = value[s];
                    t.complete(stream, value[s]);
                }
                2 => {
                    if reported[s] != 0 {
                        t.complete(stream, reported[s]); // late repeat
                    }
                }
                3 => {
                    let lag = value[s].saturating_sub(next() % 3);
                    if lag != 0 {
                        reported[s] = reported[s].max(lag);
                        t.complete(stream, lag);
                    }
                }
                _ => {}
            }
            if i % 8 == 0 {
                for s in 0..2 {
                    if value[s] != 0 {
                        reported[s] = value[s];
                        t.complete(101 + s as u64, value[s]);
                    }
                }
            }
            let st = t.stats();
            assert!(st.pending as usize <= PENDING, "{st:?}");
            assert!(st.pending <= 24, "pending {} at {i}: {st:?}", st.pending);
            assert!(st.writers <= 6, "{st:?}");
        }
        for s in 0..2 {
            t.complete(101 + s as u64, value[s]);
        }
        let st = t.stats();
        assert_eq!((st.pending, st.writers), (0, 0));
        assert_eq!(st.refused_pending + st.refused_writers, 0);
        assert!(st.pending_high <= 24, "{st:?}");
        for k in &keys {
            let s = t.snapshot(*k).unwrap();
            assert_eq!(s.completed, s.announced);
        }
    }

    #[test]
    fn values_wrap_across_u32_max() {
        // Serial comparison keeps the rule working across the wrap; zero is
        // skipped (it is the "already complete" marker, never a position).
        let mut t = big();
        let a = t.register(11).unwrap();
        let near = u32::MAX - 2;
        for v in [near, near + 1, near + 2] {
            t.publish(a, 101, v, false).unwrap();
        }
        // Across the wrap: 1 follows u32::MAX.
        t.publish(a, 101, 1, false).unwrap(); // epoch 4
        t.publish(a, 101, 2, false).unwrap(); // epoch 5
        assert_eq!(t.publish(a, 101, 2, false), Err(Error::Invalid));
        assert_eq!(t.publish(a, 101, u32::MAX, false), Err(Error::Invalid));
        t.complete(101, near + 1);
        assert_eq!(t.snapshot(a).unwrap().completed, 2);
        t.complete(101, 1); // wraps past u32::MAX: covers MAX, 1
        assert_eq!(t.snapshot(a).unwrap().completed, 4);
        assert_eq!(t.predicate(a, 5), Ok(Predicate::Pending));
        // A report of an older value cannot lower the watermark across the wrap.
        t.complete(101, near);
        assert_eq!(t.snapshot(a).unwrap().completed, 4);
        // A value behind the wrapped watermark completes on arrival (behind epoch 5).
        let epoch = t.publish(a, 202, 1, false).unwrap();
        assert_eq!(t.predicate(a, epoch), Ok(Predicate::Pending));
        t.complete(101, 2);
        assert_eq!(t.snapshot(a).unwrap().completed, 5);
        t.complete(202, 1);
        assert_eq!(t.snapshot(a).unwrap().completed, 6);
    }

    #[test]
    fn serial_comparison_is_antisymmetric_and_total_inside_the_window() {
        for (c, v) in [(5u32, 5u32), (6, 5), (1, u32::MAX), (u32::MAX, 1), (7, 1)] {
            assert_eq!(reached(c, v), c == v || later(c, v), "{c} {v}");
            assert!(!(later(c, v) && later(v, c)), "{c} {v}");
        }
        assert!(reached(1, u32::MAX));
        assert!(!reached(u32::MAX, 1));
    }

    #[test]
    fn dead_stream_discharge_frees_entries_writers_and_mark_but_not_other_streams() {
        let mut t = big();
        let a = t.register(11).unwrap();
        let b = t.register(12).unwrap();
        let c = t.register(13).unwrap();
        t.publish(a, 101, 1, false).unwrap();
        t.publish(a, 202, 1, false).unwrap(); // a also waits on 202
        t.publish(b, 101, 2, false).unwrap();
        t.publish(c, 202, 2, false).unwrap(); // survives
        assert_eq!(t.stats().pending, 4);
        assert_eq!(t.stats().writers, 4);
        assert_eq!(t.stats().marks, 2);
        t.fail_stream(101, FAILED);
        // a and b are failed and everything they held is gone, including a's
        // entry of the OTHER stream and that entry's writer.
        assert_eq!(t.predicate(a, 1), Ok(Predicate::Terminal(FAILED)));
        assert_eq!(t.predicate(b, 1), Ok(Predicate::Terminal(FAILED)));
        assert_eq!(t.predicate(c, 1), Ok(Predicate::Pending));
        let st = t.stats();
        assert_eq!((st.pending, st.writers, st.marks), (1, 1, 1));
        // The stream-202 report still completes the survivor.
        t.complete(202, 2);
        assert_eq!(t.predicate(c, 1), Ok(Predicate::Ready));
        assert_eq!(t.stats().pending, 0);
        // A late report for the dead stream finds nothing and creates nothing.
        t.complete(101, 9);
        assert_eq!(t.stats().marks, 1);
        assert_eq!(t.stats().pending, 0);
    }

    #[test]
    fn stream_churn_does_not_leak_marks_or_writers() {
        let mut t = Table::with_marks(2, 8, 4, 2).unwrap();
        for stream in 100..400u64 {
            let a = t.register(11).unwrap();
            t.publish(a, stream, 1, false).unwrap();
            t.fail_stream(stream, CANCELLED);
            let st = t.stats();
            assert_eq!((st.pending, st.writers, st.marks), (0, 0, 0), "{stream}");
            t.remove(a);
        }
        assert_eq!(t.stats().marks_full, 0);
    }

    #[test]
    fn mark_exhaustion_degrades_to_the_callers_proof() {
        let mut t = Table::with_marks(2, 8, 8, 1).unwrap();
        let a = t.register(11).unwrap();
        t.publish(a, 101, 1, false).unwrap();
        t.publish(a, 202, 1, false).unwrap(); // no mark slot for 202
        assert_eq!(t.stats().marks_full, 1);
        t.complete(202, 1); // no mark to remember it, but the entry still completes
        assert_eq!(t.snapshot(a).unwrap().completed, 0); // behind epoch 1 (prefix)
        assert_eq!(t.stats().pending, 2);
        t.complete(101, 1);
        assert_eq!(t.snapshot(a).unwrap().completed, 2);
        // The caller's proof still works for the unmarked stream.
        assert_eq!(t.publish(a, 202, 5, true), Ok(3));
        assert_eq!(t.predicate(a, 3), Ok(Predicate::Ready));
    }

    #[test]
    fn allocation_removal_and_reset_free_every_slot() {
        let mut t = big();
        let a = t.register(11).unwrap();
        let b = t.register(12).unwrap();
        for v in 1..=4 {
            t.publish(a, 101, v, false).unwrap();
            t.publish(b, 202, v, false).unwrap();
        }
        t.remove(a);
        let st = t.stats();
        assert_eq!((st.pending, st.writers), (4, 1));
        t.reset();
        let st = t.stats();
        assert_eq!((st.pending, st.writers, st.marks), (0, 0, 0));
        assert!(st.pending_high >= 8);
    }

    #[test]
    fn counters_agree_with_a_full_scan() {
        // The incremental gauges must equal what is actually in the table.
        let mut t = big();
        let mut keys: Vec<Key> = (0..4).map(|i| t.register(11 + i).unwrap()).collect();
        let mut seed = 99u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed >> 8
        };
        let mut value = [0u32; 3];
        for _ in 0..4000 {
            let s = (rnd() % 3) as usize;
            let stream = 101 + s as u64;
            match rnd() % 9 {
                0..=4 => {
                    value[s] += 1;
                    let k = keys[(rnd() % 4) as usize];
                    let _ = t.publish(k, stream, value[s], rnd() % 11 == 0);
                }
                5 | 6 => t.complete(stream, value[s].saturating_sub(rnd() % 3)),
                7 => {
                    let k = keys[(rnd() % 4) as usize];
                    let _ = t.publish(k, stream, 0, false);
                }
                _ => {
                    let roll = rnd() % 40;
                    if roll == 1 {
                        // The UMD's ABORT of one allocation.
                        let k = keys[(rnd() % 4) as usize];
                        t.terminal(k, FAILED);
                    }
                    if roll == 0 {
                        t.fail_stream(stream, FAILED);
                    }
                    if roll <= 1 {
                        // dxgkrnl destroys the failed allocations; new ones take their place.
                        for i in 0..4 {
                            if t.snapshot(keys[i]).is_ok_and(|s| s.status != LIVE) {
                                t.remove(keys[i]);
                            }
                            keys[i] = match t.find(11 + i) {
                                Some(k) => k,
                                None => t.register(11 + i).unwrap(),
                            };
                        }
                    }
                }
            }
            let st = t.stats();
            assert_eq!(st.pending as usize, t.pending.iter().flatten().count());
            assert_eq!(st.writers as usize, t.writers.iter().flatten().count());
            assert_eq!(st.marks as usize, t.marks.iter().flatten().count());
            // Every entry's writer reference is live and counted.
            for p in t.pending.iter().flatten() {
                if p.writer != NONE {
                    assert!(t.writers[p.writer].is_some_and(|w| w.stream == p.stream));
                }
            }
            for (i, w) in t.writers.iter().enumerate() {
                if let Some(w) = w {
                    let n = t.pending.iter().flatten().filter(|p| p.writer == i).count();
                    assert_eq!(w.pending as usize, n);
                    assert!(n > 0);
                }
            }
        }
    }
}
