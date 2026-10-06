//! The pipelined `ForeignFlip` host flip: a bounded window of `ScanoutFlip` messages that are
//! submitted on the control queue WITHOUT waiting for the host's reply. The pure half: the
//! window, the slot lifecycle, the acknowledgement word and its decoding. No memory, no
//! transport, no clock (every time is an argument, 100 ns units of interrupt time). The I/O half
//! is `kmd_render/src/virtio/foreign_flip.rs` (the worker's service pass),
//! `virtio/foreign_scanout.rs` (`present_submit`), `virtio/ctrl.rs` (`raw_submit_async`) and
//! `virtio/gpu/mod.rs` (`InFlightKind::RawAsync`, which writes the acknowledgement word).
//! Design, numbers and the hardware checklist: `docs/kmd-rm-client.md` 15.18 ("ForeignFlip rate
//! and pipelining").
//!
//! WHY. The synchronous flip (`present_within`) is a control-queue round trip on the HPD
//! worker, and the SAME worker drains `pending_vidpn_allocation` (every later flip's address
//! publication toward dxgkrnl) before it runs the flip service. While a round trip is in flight
//! the next publication waits for it: by the host's latency in the good case, by the whole
//! timeout (250 ms) when the host stalls. With a window the worker only SUBMITS, goes back to
//! its loop, and learns the host's answer from the acknowledgement word the used-ring drain
//! writes (and the event it signals), so programming never waits for the host.
//!
//! THE SLOT LIFECYCLE. `Free` -> `Flying` (submitted, the word is zero) -> `Free` (the word
//! carries this flip's tag: acknowledged, taken or refused). A flip whose word is still zero
//! after the timeout is `Abandoned`: counted as one failure, and it keeps its window slot,
//! because the drain still holds a pointer to the slot's word and the host may yet answer. A late
//! word is only counted. An abandoned flip that is STILL unanswered after [`ORPHAN_AFTER_100NS`]
//! (3 s) is `Orphaned`: it stops counting toward the window (a lost reply must not shrink the
//! window for good), but its CELL stays reserved so a late word cannot land in a cell a newer
//! flip uses. There are more cells ([`MAX_CELLS`]) than window slots ([`MAX_WINDOW`]), so a few
//! orphans cost nothing; only when every cell is held does a new flip take the OLDEST orphan's
//! cell (counted, [`Pipeline::recycled`]), and the tag then keeps a late word of the orphan from
//! being read as the new flip's answer (what it cannot do is save the new flip's own answer
//! from being overwritten if the orphan's word lands after it: that flip then times out, once).
//! A full window sends nothing and keeps the newest frame owed.
//!
//! BACKPRESSURE. With a window the publication of a flip toward dxgkrnl no longer waits for the
//! host, so DWM could cycle its swap chain ahead of what the host has been told. The I/O half
//! therefore does not drain the pending slot while [`Pipeline::backpressure`] (the window is full
//! of flips still within their timeout): the window bounds how many pictures run ahead of the
//! host. Abandoned flips do not hold it (the timeout bounds the wait, and the three-strike
//! fallback needs the programming to go on).
//!
//! ORDER AND STRIKES. [`Pipeline::settle_all`] returns what happened in SEQUENCE order and
//! allows at most one failure per call to cost a strike ([`Item::strike`]), the way the
//! synchronous path spends one strike per attempt and cannot spend three in one pass.
//!
//! THE WORD. One `u64` per slot, written once by the used-ring drain (any CPU, DISPATCH),
//! read by the worker: 0 = nothing yet; otherwise bit 63 set, bit 62 = "no usable reply"
//! (a transport failure or a reply shorter than a header), bits 61..32 the flip's tag (the low
//! 30 bits of its `seq`), bits 31..0 the reply header's `status` (a signed errno, 0 = taken).
//! The tag lets the worker ignore a word written by an earlier flip of the same slot (a
//! transport generation that ended between a submit and its late completion).

/// Most flips in flight at once (the window's largest size).
pub const MAX_WINDOW: usize = 4;

/// Cells (acknowledgement words, and the slots that own them): more than the window, so cells of
/// orphaned flips can stay reserved while the window is full again.
pub const MAX_CELLS: usize = 8;

/// An unanswered flip is orphaned this long after its submission (3 s, 100 ns units).
pub const ORPHAN_AFTER_100NS: u64 = 30_000_000;

/// The window the `FfAsyncWin` knob asks for: 0 = off (the synchronous flip, as it always
/// was), 1..=[`MAX_WINDOW`] the number of flips that may be in flight, anything larger is
/// [`MAX_WINDOW`].
pub const fn window_from_knob(v: u32) -> u8 {
    if v > MAX_WINDOW as u32 {
        MAX_WINDOW as u8
    } else {
        v as u8
    }
}

const VALID: u64 = 1 << 63;
const NO_REPLY: u64 = 1 << 62;
const TAG_MASK: u64 = (1 << 30) - 1;

/// The tag a flip's word carries: the low 30 bits of its `seq`.
pub const fn tag_of(seq: u64) -> u32 {
    (seq & TAG_MASK) as u32
}

/// The word for a reply whose header said `status`.
pub const fn pack_reply(tag: u32, status: i32) -> u64 {
    VALID | (((tag as u64) & TAG_MASK) << 32) | (status as u32 as u64)
}

/// The word for a flip that got no usable reply.
pub const fn pack_no_reply(tag: u32) -> u64 {
    VALID | NO_REPLY | (((tag as u64) & TAG_MASK) << 32)
}

/// How the host answered one flip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ack {
    /// Status 0: the host took the flip.
    Taken,
    /// A reply with a nonzero status (a signed errno).
    Refused(i32),
    /// No usable reply (the transport failed, or the reply was shorter than a header).
    NoReply,
}

impl Ack {
    pub const fn taken(self) -> bool {
        matches!(self, Ack::Taken)
    }
}

/// Decode `word` as the acknowledgement of flip `seq`: `None` when nothing was written yet or
/// the word belongs to another flip (a different tag).
pub const fn unpack(word: u64, seq: u64) -> Option<Ack> {
    if word & VALID == 0 {
        return None;
    }
    if ((word >> 32) & TAG_MASK) != (tag_of(seq) as u64) {
        return None;
    }
    if word & NO_REPLY != 0 {
        return Some(Ack::NoReply);
    }
    let status = word as u32 as i32;
    if status == 0 {
        Some(Ack::Taken)
    } else {
        Some(Ack::Refused(status))
    }
}

/// Microseconds from `at` to `now` (100 ns units), saturated to `u32`.
pub const fn elapsed_us(at: u64, now: u64) -> u32 {
    let us = now.saturating_sub(at) / 10;
    if us > u32::MAX as u64 {
        u32::MAX
    } else {
        us as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Free,
    /// Submitted; the word is not written yet and the timeout has not passed.
    Flying,
    /// Timed out and counted; still holds its window slot until the word arrives or the orphan
    /// age passes.
    Abandoned,
    /// Unanswered for [`ORPHAN_AFTER_100NS`]: not counted toward the window, the cell stays
    /// reserved until its word arrives (or a full cell array recycles the oldest).
    Orphaned,
}

/// One flip of the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    pub state: SlotState,
    pub seq: u64,
    /// When it was submitted (100 ns).
    pub at: u64,
    /// The arbiter generation the flip was minted in (`foreign_scanout_flip_done`).
    pub generation: u32,
    /// The transport epoch (`nvrm_epoch`) the presenter was in: an answer of an earlier epoch
    /// is not the new presenter's to count.
    pub epoch: u64,
    /// The presenter's slot and whether it was a frame (`copied`) or a resume, for the
    /// failure it may owe.
    pub pslot: u8,
    pub copied: bool,
}

impl Slot {
    const FREE: Slot = Slot {
        state: SlotState::Free,
        seq: 0,
        at: 0,
        generation: 0,
        epoch: 0,
        pslot: 0,
        copied: false,
    };
}

/// What one look at a slot found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Settled {
    /// Nothing to do (free, or still waiting within the timeout).
    Pending,
    /// The host answered a flip that was within its timeout; the slot is free again.
    Acked { slot: Slot, ack: Ack },
    /// The timeout passed with no word: one failure, the slot stays held.
    TimedOut { slot: Slot },
    /// The word of an abandoned or orphaned flip arrived; the slot is free again. Counted only.
    Late { slot: Slot, ack: Ack },
    /// An abandoned flip aged out: it no longer counts toward the window. Counted only.
    Orphaned { slot: Slot },
}

impl Settled {
    /// The flip's `seq` (0 for `Pending`): the order a batch is settled in.
    pub const fn seq(&self) -> u64 {
        match self {
            Settled::Pending => 0,
            Settled::Acked { slot, .. }
            | Settled::TimedOut { slot }
            | Settled::Late { slot, .. }
            | Settled::Orphaned { slot } => slot.seq,
        }
    }

    /// Whether this is a failure of a flip that was within its time (a refusal, no reply, or the
    /// timeout): the only things that may cost the presenter a strike.
    pub const fn is_failure(&self) -> bool {
        match self {
            Settled::Acked { ack, .. } => !ack.taken(),
            Settled::TimedOut { .. } => true,
            _ => false,
        }
    }
}

/// One settled flip of a batch, with whether it is the one failure of the batch that costs a
/// strike.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    pub settled: Settled,
    pub strike: bool,
}

/// What one [`Pipeline::settle_all`] found, in sequence order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlements {
    items: [Option<Item>; MAX_CELLS],
    len: usize,
}

impl Settlements {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = Item> + '_ {
        self.items[..self.len].iter().filter_map(|i| *i)
    }
}

/// The window. Plain data, owned by the HPD worker (a leaf lock in the I/O half).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pipeline {
    window: u8,
    slots: [Slot; MAX_CELLS],
    /// Most flips holding window slots at once since the last [`Self::reset`].
    high: u8,
    /// Orphaned cells taken over by a newer flip.
    recycled: u32,
}

impl Pipeline {
    pub const fn new() -> Pipeline {
        Pipeline {
            window: 0,
            slots: [Slot::FREE; MAX_CELLS],
            high: 0,
            recycled: 0,
        }
    }

    /// Everything is forgotten (the transport generation ended: no completion can come).
    pub fn reset(&mut self) {
        *self = Pipeline::new();
    }

    /// The window in force. 0 turns the pipeline off; flips already in flight stay in their
    /// slots and are settled as usual.
    pub fn set_window(&mut self, window: u8) {
        self.window = window.min(MAX_WINDOW as u8);
    }

    pub fn window(&self) -> u8 {
        self.window
    }

    fn count(&self, f: impl Fn(SlotState) -> bool) -> usize {
        self.slots.iter().filter(|s| f(s.state)).count()
    }

    /// Cells held by any flip, orphans included (the worker settles while this is nonzero).
    pub fn occupied(&self) -> usize {
        self.count(|s| s != SlotState::Free)
    }

    /// Window slots in use: flying or abandoned (orphans do not count).
    pub fn held(&self) -> usize {
        self.count(|s| matches!(s, SlotState::Flying | SlotState::Abandoned))
    }

    /// Flips submitted and not yet answered or timed out.
    pub fn flying(&self) -> usize {
        self.count(|s| s == SlotState::Flying)
    }

    /// Orphaned cells.
    pub fn orphans(&self) -> usize {
        self.count(|s| s == SlotState::Orphaned)
    }

    /// Whether a flip may be submitted now: the window is on and not full.
    pub fn can_submit(&self) -> bool {
        self.window != 0 && self.held() < usize::from(self.window)
    }

    /// The window is on and full of flips still within their timeout: the I/O half leaves the
    /// pending programming slot alone, so DWM cannot cycle further ahead of the host than the
    /// window. Answers (which wake the worker) and the timeout end it.
    pub fn backpressure(&self) -> bool {
        self.window != 0 && self.flying() >= usize::from(self.window)
    }

    /// Most window slots in use at once.
    pub fn high_water(&self) -> u8 {
        self.high
    }

    /// Orphaned cells a newer flip took over.
    pub fn recycled(&self) -> u32 {
        self.recycled
    }

    /// The window is on and every slot of it holds a flip the host never answered (all
    /// abandoned, none flying): nothing will free a slot until a late answer arrives or the
    /// orphan age passes, so the caller must count a failure per attempt (as the round trip's
    /// timeout did) instead of waiting for a completion that may never come.
    pub fn stuck(&self) -> bool {
        self.window != 0 && self.held() >= usize::from(self.window) && self.flying() == 0
    }

    /// Whether any cell is held (the I/O half must keep settling while this is true, whatever
    /// the knob says).
    pub fn busy(&self) -> bool {
        self.occupied() != 0
    }

    /// The cell the next flip would take: a free one, else (every cell held) the oldest
    /// orphan's; `None` when the window is full. The caller needs the index BEFORE it submits
    /// (the cell is what the submit hands the transport).
    pub fn free_slot(&self) -> Option<usize> {
        if !self.can_submit() {
            return None;
        }
        if let Some(i) = self.slots.iter().position(|s| s.state == SlotState::Free) {
            return Some(i);
        }
        self.slots
            .iter()
            .enumerate()
            .filter(|(_, s)| s.state == SlotState::Orphaned)
            .min_by_key(|(_, s)| s.at)
            .map(|(i, _)| i)
    }

    /// Record flip `seq`, submitted at `at`, in cell `i` (from [`Self::free_slot`], with no
    /// other change in between: the worker is the only caller). The caller zeroed the cell's
    /// word before the message could reach the ring.
    #[allow(clippy::too_many_arguments)]
    pub fn begin_at(
        &mut self,
        i: usize,
        seq: u64,
        at: u64,
        generation: u32,
        epoch: u64,
        pslot: u8,
        copied: bool,
    ) -> bool {
        match self.slots.get(i) {
            Some(s) if s.state == SlotState::Free => {}
            Some(s) if s.state == SlotState::Orphaned => {
                self.recycled = self.recycled.wrapping_add(1);
            }
            _ => return false,
        }
        self.slots[i] = Slot {
            state: SlotState::Flying,
            seq,
            at,
            generation,
            epoch,
            pslot,
            copied,
        };
        let n = self.held() as u8;
        if n > self.high {
            self.high = n;
        }
        true
    }

    /// [`Self::free_slot`] and [`Self::begin_at`] in one (epoch 0).
    pub fn begin(
        &mut self,
        seq: u64,
        at: u64,
        generation: u32,
        pslot: u8,
        copied: bool,
    ) -> Option<usize> {
        let i = self.free_slot()?;
        self.begin_at(i, seq, at, generation, 0, pslot, copied)
            .then_some(i)
    }

    /// The submit failed before the message reached the ring: give the cell back.
    pub fn cancel(&mut self, i: usize) {
        if let Some(s) = self.slots.get_mut(i) {
            *s = Slot::FREE;
        }
    }

    pub fn slot(&self, i: usize) -> Option<&Slot> {
        self.slots.get(i)
    }

    /// Look at cell `i`, whose word reads `word` at `now`; `timeout` is how long a host gets
    /// (100 ns).
    pub fn settle(&mut self, i: usize, word: u64, now: u64, timeout: u64) -> Settled {
        let Some(&s) = self.slots.get(i) else {
            return Settled::Pending;
        };
        match s.state {
            SlotState::Free => Settled::Pending,
            SlotState::Flying => {
                if let Some(ack) = unpack(word, s.seq) {
                    self.slots[i] = Slot::FREE;
                    Settled::Acked { slot: s, ack }
                } else if now.saturating_sub(s.at) >= timeout {
                    self.slots[i].state = SlotState::Abandoned;
                    Settled::TimedOut { slot: s }
                } else {
                    Settled::Pending
                }
            }
            SlotState::Abandoned | SlotState::Orphaned => match unpack(word, s.seq) {
                Some(ack) => {
                    self.slots[i] = Slot::FREE;
                    Settled::Late { slot: s, ack }
                }
                None => {
                    if s.state == SlotState::Abandoned
                        && now.saturating_sub(s.at) >= ORPHAN_AFTER_100NS
                    {
                        self.slots[i].state = SlotState::Orphaned;
                        Settled::Orphaned { slot: s }
                    } else {
                        Settled::Pending
                    }
                }
            },
        }
    }

    /// Settle every cell against `words` (one read of each, taken by the caller AFTER it drained
    /// the used ring) and return what happened in SEQUENCE order, so the presenter sees the
    /// flips in the order they were sent whatever cell they used. At most ONE failure of the
    /// batch is marked to cost a strike (the first in sequence order): a pass cannot spend
    /// three strikes at once, as the synchronous path cannot.
    pub fn settle_all(&mut self, words: &[u64; MAX_CELLS], now: u64, timeout: u64) -> Settlements {
        let mut out = Settlements {
            items: [None; MAX_CELLS],
            len: 0,
        };
        for (i, &w) in words.iter().enumerate() {
            let r = self.settle(i, w, now, timeout);
            if r != Settled::Pending {
                out.items[out.len] = Some(Item {
                    settled: r,
                    strike: false,
                });
                out.len += 1;
            }
        }
        // Insertion sort by seq (at most MAX_CELLS items; no allocation).
        for a in 1..out.len {
            let mut b = a;
            while b > 0 && seq_of(&out.items[b - 1]) > seq_of(&out.items[b]) {
                out.items.swap(b - 1, b);
                b -= 1;
            }
        }
        if let Some(first) = out.items[..out.len]
            .iter_mut()
            .flatten()
            .find(|i| i.settled.is_failure())
        {
            first.strike = true;
        }
        out
    }

    /// When the oldest flying flip times out (100 ns), for the worker's timed wake.
    pub fn next_deadline(&self, timeout: u64) -> Option<u64> {
        self.slots
            .iter()
            .filter(|s| s.state == SlotState::Flying)
            .map(|s| s.at.saturating_add(timeout))
            .min()
    }
}

fn seq_of(i: &Option<Item>) -> u64 {
    i.map_or(0, |i| i.settled.seq())
}

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;

    const MS: u64 = 10_000;
    const T: u64 = 250 * MS;

    fn pipe(w: u8) -> Pipeline {
        let mut p = Pipeline::new();
        p.set_window(w);
        p
    }

    #[test]
    fn the_knob_maps_to_a_window() {
        assert_eq!(window_from_knob(0), 0);
        assert_eq!(window_from_knob(1), 1);
        assert_eq!(window_from_knob(4), 4);
        assert_eq!(window_from_knob(5), 4);
        assert_eq!(window_from_knob(u32::MAX), 4);
    }

    #[test]
    fn a_word_round_trips_and_belongs_to_one_flip() {
        let seq = (1u64 << 40) + 77;
        assert_eq!(unpack(0, seq), None);
        assert_eq!(unpack(pack_reply(tag_of(seq), 0), seq), Some(Ack::Taken));
        assert_eq!(
            unpack(pack_reply(tag_of(seq), -9), seq),
            Some(Ack::Refused(-9))
        );
        assert_eq!(
            unpack(pack_reply(tag_of(seq), i32::MIN), seq),
            Some(Ack::Refused(i32::MIN))
        );
        assert_eq!(unpack(pack_no_reply(tag_of(seq)), seq), Some(Ack::NoReply));
        // Another flip's word is not this one's.
        assert_eq!(unpack(pack_reply(tag_of(seq + 1), 0), seq), None);
        assert_eq!(unpack(pack_no_reply(tag_of(seq + 1)), seq), None);
        // The tag is 30 bits of the seq; a larger tag is masked, never spilling into the flags.
        assert_eq!(pack_reply(u32::MAX, 0) & NO_REPLY, 0);
        assert_eq!(tag_of(u64::MAX), (1 << 30) - 1);
        // A reply with status 0 is never mistaken for "nothing written".
        assert_ne!(pack_reply(0, 0), 0);
    }

    #[test]
    fn the_window_bounds_what_is_in_flight() {
        let mut p = pipe(2);
        assert!(p.can_submit());
        let a = p.begin(1, 10, 7, 0, true).unwrap();
        let b = p.begin(2, 20, 7, 0, true).unwrap();
        assert_ne!(a, b);
        assert!(!p.can_submit());
        assert_eq!(p.begin(3, 30, 7, 0, true), None);
        assert_eq!(p.flying(), 2);
        assert_eq!(p.high_water(), 2);
        // The first answer frees exactly its own slot.
        let w = pack_reply(tag_of(1), 0);
        assert!(matches!(
            p.settle(a, w, 100, T),
            Settled::Acked {
                ack: Ack::Taken,
                slot: Slot { seq: 1, .. }
            }
        ));
        assert!(p.can_submit());
        assert_eq!(p.begin(3, 40, 7, 0, true), Some(a));
    }

    #[test]
    fn a_window_of_zero_submits_nothing() {
        let mut p = pipe(0);
        assert!(!p.can_submit());
        assert_eq!(p.begin(1, 10, 0, 0, true), None);
        assert!(!p.busy());
    }

    #[test]
    fn a_flip_is_pending_until_its_word_or_its_timeout() {
        let mut p = pipe(1);
        let i = p.begin(5, 1_000, 3, 0, false).unwrap();
        assert_eq!(p.settle(i, 0, 1_000 + T - 1, T), Settled::Pending);
        assert_eq!(p.next_deadline(T), Some(1_000 + T));
        match p.settle(i, 0, 1_000 + T, T) {
            Settled::TimedOut { slot } => {
                assert_eq!(slot.seq, 5);
                assert!(!slot.copied);
                assert_eq!(slot.generation, 3);
            }
            other => panic!("{other:?}"),
        }
        // Timed out once: it is counted once, and no longer sets a deadline.
        assert_eq!(p.settle(i, 0, 1_000 + 2 * T, T), Settled::Pending);
        assert_eq!(p.next_deadline(T), None);
    }

    #[test]
    fn an_abandoned_slot_stays_held_until_its_word_arrives() {
        let mut p = pipe(1);
        let i = p.begin(5, 0, 0, 0, true).unwrap();
        assert!(matches!(p.settle(i, 0, T, T), Settled::TimedOut { .. }));
        // The window is full of a flip nobody answers: nothing more is submitted.
        assert!(!p.can_submit());
        assert!(p.busy());
        assert_eq!(p.begin(6, T, 0, 0, true), None);
        // A word of ANOTHER flip does not free it.
        assert_eq!(
            p.settle(i, pack_reply(tag_of(6), 0), 2 * T, T),
            Settled::Pending
        );
        assert!(!p.can_submit());
        // Its own word does, as a late answer (counted, not a failure).
        match p.settle(i, pack_reply(tag_of(5), 0), 3 * T, T) {
            Settled::Late { slot, ack } => {
                assert_eq!(slot.seq, 5);
                assert_eq!(ack, Ack::Taken);
            }
            other => panic!("{other:?}"),
        }
        assert!(p.can_submit());
    }

    #[test]
    fn a_window_of_unanswered_flips_is_stuck_not_merely_busy() {
        let mut p = pipe(2);
        assert!(!p.stuck());
        let a = p.begin(1, 0, 0, 0, true).unwrap();
        let b = p.begin(2, 0, 0, 0, true).unwrap();
        // Full but flying: busy, not stuck.
        assert!(!p.stuck());
        assert!(matches!(p.settle(a, 0, T, T), Settled::TimedOut { .. }));
        assert!(!p.stuck(), "one still flies");
        assert!(matches!(p.settle(b, 0, T, T), Settled::TimedOut { .. }));
        assert!(p.stuck());
        // A late answer frees a slot: no longer stuck.
        assert!(matches!(
            p.settle(a, pack_reply(tag_of(1), 0), 2 * T, T),
            Settled::Late { .. }
        ));
        assert!(!p.stuck());
        // A window that is off is never stuck.
        p.set_window(0);
        assert!(!p.stuck());
    }

    #[test]
    fn an_answer_that_races_the_timeout_is_an_answer() {
        // The word is read first: a reply that landed just before the deadline check wins.
        let mut p = pipe(1);
        let i = p.begin(9, 0, 0, 0, true).unwrap();
        let w = pack_reply(tag_of(9), -5);
        assert!(matches!(
            p.settle(i, w, 10 * T, T),
            Settled::Acked {
                ack: Ack::Refused(-5),
                ..
            }
        ));
    }

    #[test]
    fn a_refused_or_missing_reply_is_not_taken() {
        let mut p = pipe(2);
        let a = p.begin(1, 0, 0, 0, true).unwrap();
        let b = p.begin(2, 0, 0, 0, true).unwrap();
        match p.settle(a, pack_no_reply(tag_of(1)), 1, T) {
            Settled::Acked { ack, .. } => assert!(!ack.taken()),
            other => panic!("{other:?}"),
        }
        match p.settle(b, pack_reply(tag_of(2), -22), 1, T) {
            Settled::Acked { ack, .. } => assert!(!ack.taken()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_deadline_is_the_oldest_flying_flip() {
        let mut p = pipe(3);
        p.begin(1, 500, 0, 0, true).unwrap();
        p.begin(2, 100, 0, 0, true).unwrap();
        p.begin(3, 900, 0, 0, true).unwrap();
        assert_eq!(p.next_deadline(T), Some(100 + T));
    }

    #[test]
    fn cancel_gives_the_slot_back_and_reset_forgets_everything() {
        let mut p = pipe(1);
        let i = p.begin(1, 0, 0, 0, true).unwrap();
        p.cancel(i);
        assert!(p.can_submit());
        let _ = p.begin(2, 0, 0, 0, true).unwrap();
        p.reset();
        assert!(!p.busy());
        assert_eq!(p.window(), 0);
        assert_eq!(p.high_water(), 0);
    }

    #[test]
    fn lowering_the_window_does_not_lose_a_flip_in_flight() {
        let mut p = pipe(3);
        let a = p.begin(1, 0, 0, 0, true).unwrap();
        let _b = p.begin(2, 0, 0, 0, true).unwrap();
        p.set_window(0);
        assert!(!p.can_submit());
        assert!(p.busy());
        assert!(matches!(
            p.settle(a, pack_reply(tag_of(1), 0), 1, T),
            Settled::Acked { .. }
        ));
        assert!(p.busy());
    }

    #[test]
    fn the_slot_is_chosen_before_the_submit_and_filled_after_it() {
        let mut p = pipe(2);
        // The submit hands the transport slot `i`'s word before the flip's seq exists.
        let i = p.free_slot().unwrap();
        assert_eq!(p.occupied(), 0);
        assert!(p.begin_at(i, 7, 100, 4, 9, 1, true));
        let s = *p.slot(i).unwrap();
        assert_eq!(
            (s.seq, s.at, s.generation, s.epoch, s.pslot),
            (7, 100, 4, 9, 1)
        );
        assert_eq!(s.state, SlotState::Flying);
        // The same slot cannot be filled twice, and the next free one is another.
        assert!(!p.begin_at(i, 8, 200, 4, 9, 1, true));
        let j = p.free_slot().unwrap();
        assert_ne!(i, j);
        assert!(p.begin_at(j, 8, 200, 4, 9, 1, true));
        assert_eq!(p.free_slot(), None);
        // The epoch rides to the answer.
        match p.settle(i, pack_reply(tag_of(7), 0), 300, T) {
            Settled::Acked { slot, .. } => assert_eq!(slot.epoch, 9),
            other => panic!("{other:?}"),
        }
    }

    // ---- orphaning ---------------------------------------------------------------------

    const ORPHAN: u64 = ORPHAN_AFTER_100NS;

    #[test]
    fn a_lost_reply_stops_shrinking_the_window_after_the_orphan_age() {
        let mut p = pipe(1);
        let i = p.begin(1, 0, 0, 0, true).unwrap();
        assert!(matches!(p.settle(i, 0, T, T), Settled::TimedOut { .. }));
        // Abandoned: still holds the window (and is stuck, so each attempt is a failure).
        assert!(!p.can_submit());
        assert!(p.stuck());
        assert_eq!(p.settle(i, 0, ORPHAN - 1, T), Settled::Pending);
        assert!(!p.can_submit());
        // Aged out: orphaned, the window is open again, the cell is still reserved.
        assert!(matches!(
            p.settle(i, 0, ORPHAN, T),
            Settled::Orphaned {
                slot: Slot { seq: 1, .. }
            }
        ));
        assert!(p.can_submit());
        assert!(!p.stuck());
        assert_eq!(p.held(), 0);
        assert_eq!(p.orphans(), 1);
        assert!(p.busy(), "the cell is reserved until its word arrives");
        // Orphaned once, not every look.
        assert_eq!(p.settle(i, 0, 2 * ORPHAN, T), Settled::Pending);
        // A new flip does NOT take the orphan's cell while another is free.
        let j = p.free_slot().unwrap();
        assert_ne!(i, j);
        assert!(p.begin_at(j, 2, ORPHAN, 0, 0, 0, true));
        assert_eq!(p.recycled(), 0);
        // The orphan's late word frees its own cell and is only counted.
        match p.settle(i, pack_reply(tag_of(1), 0), 3 * ORPHAN, T) {
            Settled::Late { slot, ack } => {
                assert_eq!(slot.seq, 1);
                assert!(ack.taken());
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(p.orphans(), 0);
    }

    #[test]
    fn a_late_word_never_answers_a_newer_flip_of_a_recycled_cell() {
        // Window 4, 8 cells: orphan every cell so the next flip must recycle the oldest.
        let mut p = pipe(4);
        let mut cells = std::vec::Vec::new();
        for n in 0..MAX_CELLS as u64 {
            // Orphan each before the next (the window never holds more than it allows).
            let i = p.free_slot().unwrap();
            assert!(p.begin_at(i, 100 + n, n, 0, 0, 0, true));
            assert!(matches!(p.settle(i, 0, n + T, T), Settled::TimedOut { .. }));
            assert!(matches!(
                p.settle(i, 0, n + ORPHAN, T),
                Settled::Orphaned { .. }
            ));
            cells.push(i);
        }
        assert_eq!(p.orphans(), MAX_CELLS);
        assert_eq!(p.held(), 0);
        // Every cell is reserved: the OLDEST orphan's is taken over, and counted.
        let i = p.free_slot().unwrap();
        assert_eq!(i, cells[0]);
        assert!(p.begin_at(i, 500, 10 * ORPHAN, 0, 0, 0, true));
        assert_eq!(p.recycled(), 1);
        assert_eq!(p.orphans(), MAX_CELLS - 1);
        // The old orphan's late word (its tag) sits in the cell: it is NOT this flip's answer.
        let stale = pack_reply(tag_of(100), 0);
        assert_eq!(p.settle(i, stale, 10 * ORPHAN + 1, T), Settled::Pending);
        // The new flip's own word is.
        assert!(matches!(
            p.settle(i, pack_reply(tag_of(500), 0), 10 * ORPHAN + 2, T),
            Settled::Acked { .. }
        ));
    }

    #[test]
    fn a_dead_host_costs_the_window_three_seconds_not_forever() {
        let mut p = pipe(2);
        let a = p.begin(1, 0, 0, 0, true).unwrap();
        let b = p.begin(2, 1, 0, 0, true).unwrap();
        for i in [a, b] {
            assert!(matches!(p.settle(i, 0, 2 * T, T), Settled::TimedOut { .. }));
        }
        assert!(p.stuck());
        let batch = p.settle_all(&[0; MAX_CELLS], ORPHAN + 1, T);
        assert_eq!(batch.len(), 2);
        assert!(batch
            .iter()
            .all(|i| matches!(i.settled, Settled::Orphaned { .. }) && !i.strike));
        assert!(p.can_submit());
        assert_eq!(p.held(), 0);
    }

    // ---- backpressure ------------------------------------------------------------------

    #[test]
    fn backpressure_is_a_window_of_flips_still_within_their_time() {
        let mut p = pipe(2);
        assert!(!p.backpressure());
        let a = p.begin(1, 0, 0, 0, true).unwrap();
        assert!(!p.backpressure());
        let b = p.begin(2, 0, 0, 0, true).unwrap();
        assert!(p.backpressure());
        // One answer opens it.
        assert!(matches!(
            p.settle(a, pack_reply(tag_of(1), 0), 10, T),
            Settled::Acked { .. }
        ));
        assert!(!p.backpressure());
        // A flip that timed out does not hold the programming back (the three-strike
        // fallback needs it to go on).
        let a = p.begin(3, 20, 0, 0, true).unwrap();
        assert!(p.backpressure());
        assert!(matches!(
            p.settle(a, 0, 20 + T, T),
            Settled::TimedOut { .. }
        ));
        assert!(matches!(
            p.settle(b, 0, 20 + T, T),
            Settled::TimedOut { .. }
        ));
        assert!(!p.backpressure());
        // Off: never.
        let mut off = pipe(0);
        assert!(!off.backpressure());
        off.set_window(0);
        assert!(!off.backpressure());
    }

    // ---- order and strikes -------------------------------------------------------------

    fn failing_word(seq: u64) -> u64 {
        pack_reply(tag_of(seq), -5)
    }

    #[test]
    fn a_batch_is_settled_in_sequence_order_whatever_the_cell_order() {
        let mut p = pipe(3);
        // Cell 0 gets the NEWEST flip: `free_slot` order is not sequence order after a reuse.
        let c0 = p.begin(30, 0, 0, 0, true).unwrap();
        let c1 = p.begin(10, 0, 0, 0, true).unwrap();
        let c2 = p.begin(20, 0, 0, 0, true).unwrap();
        assert_eq!((c0, c1, c2), (0, 1, 2));
        let mut w = [0u64; MAX_CELLS];
        w[c0] = pack_reply(tag_of(30), 0);
        w[c1] = failing_word(10);
        w[c2] = pack_reply(tag_of(20), 0);
        let batch = p.settle_all(&w, 100, T);
        let seqs: std::vec::Vec<u64> = batch.iter().map(|i| i.settled.seq()).collect();
        assert_eq!(seqs, [10, 20, 30]);
        // The failure is first: the later acks come after it, so they clear its strike the
        // way the synchronous path's next success would.
        let first = batch.iter().next().unwrap();
        assert!(first.strike);
        assert!(batch.iter().skip(1).all(|i| !i.strike));
    }

    #[test]
    fn a_pass_spends_at_most_one_strike_however_many_flips_failed() {
        // Three failures answered together: one strike, as `three_strikes_cannot_be_spent_in_
        // one_pass` for the synchronous path.
        let mut p = pipe(4);
        let cells: std::vec::Vec<usize> = (1..=4u64)
            .map(|n| p.begin(n, 0, 0, 0, true).unwrap())
            .collect();
        let mut w = [0u64; MAX_CELLS];
        w[cells[0]] = failing_word(1);
        w[cells[1]] = pack_no_reply(tag_of(2));
        w[cells[2]] = failing_word(3);
        // Cell 3 times out in the same call.
        let batch = p.settle_all(&w, T, T);
        assert_eq!(batch.len(), 4);
        assert_eq!(batch.iter().filter(|i| i.settled.is_failure()).count(), 4);
        assert_eq!(batch.iter().filter(|i| i.strike).count(), 1);

        // Drive a presenter the way the I/O half does: a strike only where marked.
        let mut pr = crate::rm_present::Presenter::new(1);
        let now = 1_000_000;
        for it in batch.iter() {
            if it.strike {
                pr.flipped(0, true, crate::rm_present::FlipResult::Failed, now);
            }
        }
        assert_eq!(pr.fails(), 1);
        assert!(!pr.gave_up());
        // The next failures come in later passes, and only the third gives up.
        for n in 2..=crate::rm_present::MAX_CONSECUTIVE_FAILS {
            pr.flipped(
                0,
                true,
                crate::rm_present::FlipResult::Failed,
                now + u64::from(n),
            );
        }
        assert!(pr.gave_up());
    }

    #[test]
    fn a_later_taken_flip_clears_an_earlier_strike_only_in_sequence_order() {
        // Sequence order: failure of 5, then the ack of 6: the ack clears (the synchronous
        // path's next success would). Never the other way round: an ack of an OLDER flip
        // processed after a newer failure would erase a strike that is still owed.
        let mut p = pipe(2);
        let a = p.begin(5, 0, 0, 0, true).unwrap();
        let b = p.begin(6, 0, 0, 0, true).unwrap();
        let mut w = [0u64; MAX_CELLS];
        // The OLDER flip is the one acked, the NEWER one failed; cell order says the reverse.
        w[a] = pack_reply(tag_of(5), 0);
        w[b] = failing_word(6);
        let batch = p.settle_all(&w, 1, T);
        let order: std::vec::Vec<(u64, bool)> =
            batch.iter().map(|i| (i.settled.seq(), i.strike)).collect();
        assert_eq!(order, [(5, false), (6, true)]);
        // Replayed on a presenter: the ack of 5 first, then the failure of 6 stands.
        let mut pr = crate::rm_present::Presenter::new(1);
        for it in batch.iter() {
            match it.settled {
                Settled::Acked { ack, .. } if ack.taken() => pr.acked(),
                _ if it.strike => pr.flipped(0, true, crate::rm_present::FlipResult::Failed, 1_000),
                _ => {}
            }
        }
        assert_eq!(pr.fails(), 1);
    }

    #[test]
    fn late_answers_and_orphans_never_cost_a_strike() {
        let mut p = pipe(1);
        let i = p.begin(1, 0, 0, 0, true).unwrap();
        assert!(matches!(p.settle(i, 0, T, T), Settled::TimedOut { .. }));
        // A late REFUSAL is not a failure of a flip within its time.
        let batch = p.settle_all(
            &{
                let mut w = [0u64; MAX_CELLS];
                w[i] = failing_word(1);
                w
            },
            2 * T,
            T,
        );
        assert_eq!(batch.len(), 1);
        assert!(!batch.iter().next().unwrap().strike);
        assert!(!batch.iter().next().unwrap().settled.is_failure());
    }

    #[test]
    fn elapsed_is_in_microseconds_and_saturates() {
        assert_eq!(elapsed_us(0, 10), 1);
        assert_eq!(elapsed_us(100, 50), 0);
        assert_eq!(elapsed_us(0, u64::MAX), u32::MAX);
    }
}
