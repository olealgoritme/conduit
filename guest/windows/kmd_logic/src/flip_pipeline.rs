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
//! after the timeout is `Abandoned`: counted as one failure, and its slot stays OCCUPIED until
//! its word finally arrives (or the transport generation ends), because the drain still holds
//! a pointer to the slot's word and writing a reused one would corrupt a newer flip. A late
//! word is only counted. So a silent host shrinks the window, it never corrupts it; a full
//! window sends nothing and keeps the newest frame owed.
//!
//! THE WORD. One `u64` per slot, written once by the used-ring drain (any CPU, DISPATCH),
//! read by the worker: 0 = nothing yet; otherwise bit 63 set, bit 62 = "no usable reply"
//! (a transport failure or a reply shorter than a header), bits 61..32 the flip's tag (the low
//! 30 bits of its `seq`), bits 31..0 the reply header's `status` (a signed errno, 0 = taken).
//! The tag lets the worker ignore a word written by an earlier flip of the same slot (a
//! transport generation that ended between a submit and its late completion).

/// Most flips in flight at once (the size of the slot and word arrays).
pub const MAX_WINDOW: usize = 4;

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
    /// Timed out and counted; still holds the slot until the word arrives.
    Abandoned,
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
    /// The timeout passed with no word: one failure, the slot stays occupied.
    TimedOut { slot: Slot },
    /// The word of an abandoned flip arrived; the slot is free again. Counted only.
    Late { slot: Slot, ack: Ack },
}

/// The window. Plain data, owned by the HPD worker (a leaf lock in the I/O half).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pipeline {
    window: u8,
    slots: [Slot; MAX_WINDOW],
    /// Most flips occupying slots at once since the last [`Self::reset`].
    high: u8,
}

impl Pipeline {
    pub const fn new() -> Pipeline {
        Pipeline {
            window: 0,
            slots: [Slot::FREE; MAX_WINDOW],
            high: 0,
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

    /// Slots held by a flip (flying or abandoned).
    pub fn occupied(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state != SlotState::Free)
            .count()
    }

    /// Flips submitted and not yet answered or timed out.
    pub fn flying(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.state == SlotState::Flying)
            .count()
    }

    /// Whether a flip may be submitted now: the window is on and not full.
    pub fn can_submit(&self) -> bool {
        self.window != 0 && self.occupied() < usize::from(self.window)
    }

    /// Most slots occupied at once.
    pub fn high_water(&self) -> u8 {
        self.high
    }

    /// The window is on and every slot of it holds a flip the host never answered (all
    /// abandoned, none flying): nothing will free a slot until a late answer arrives, so the
    /// caller must count a failure per attempt (as the round trip's timeout did) instead of
    /// waiting for a completion that may never come.
    pub fn stuck(&self) -> bool {
        self.window != 0 && self.occupied() >= usize::from(self.window) && self.flying() == 0
    }

    /// Whether any slot is held (the I/O half must keep settling while this is true, whatever
    /// the knob says).
    pub fn busy(&self) -> bool {
        self.occupied() != 0
    }

    /// The slot the next flip would take: free, with the window not full. The caller needs the
    /// index BEFORE it submits (the slot's word is what the submit hands the transport).
    pub fn free_slot(&self) -> Option<usize> {
        if !self.can_submit() {
            return None;
        }
        self.slots.iter().position(|s| s.state == SlotState::Free)
    }

    /// Record flip `seq`, submitted at `at`, in slot `i` (from [`Self::free_slot`], with no
    /// other change in between: the worker is the only caller). The caller zeroed the slot's
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
        let n = self.occupied() as u8;
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

    /// The submit failed before the message reached the ring: give the slot back.
    pub fn cancel(&mut self, i: usize) {
        if let Some(s) = self.slots.get_mut(i) {
            *s = Slot::FREE;
        }
    }

    pub fn slot(&self, i: usize) -> Option<&Slot> {
        self.slots.get(i)
    }

    /// Look at slot `i`, whose word reads `word` at `now`; `timeout` is how long a host gets
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
            SlotState::Abandoned => match unpack(word, s.seq) {
                Some(ack) => {
                    self.slots[i] = Slot::FREE;
                    Settled::Late { slot: s, ack }
                }
                None => Settled::Pending,
            },
        }
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

impl Default for Pipeline {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn elapsed_is_in_microseconds_and_saturates() {
        assert_eq!(elapsed_us(0, 10), 1);
        assert_eq!(elapsed_us(100, 50), 0);
        assert_eq!(elapsed_us(0, u64::MAX), u32::MAX);
    }
}
