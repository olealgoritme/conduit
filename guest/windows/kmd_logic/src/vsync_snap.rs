//! The heartbeat's exact (count, time) pair (`docs/kmd-rm-client.md` 15.18.16).
//!
//! A rate computed from two reads of the registry mirror is only as good as the pair each read
//! returns. The mirror writes a hundred values one after the other, ticks keep arriving between
//! them, and each value is its own registry transaction, so `VsTickN` and the time a reader divides
//! by (`StallT`, `VsLiveT`, `VpDmpT`, its own clock) belong to different instants: on a window of
//! ten seconds that is a few tenths of a percent from the write order alone, and several percent
//! when the reader's divisor is the time it ASKED rather than the time the mirror TOOK its sample.
//! This module makes one sample whole:
//!
//! * the tick callback (DISPATCH, the only writer) stores the running totals and the interrupt time
//!   of THIS tick in one seqlock cell ([`Snap`]): tick count, time (100 ns), period slots the
//!   chain moved over, slots dropped because the callback ran late, catch-up ticks;
//! * the mirror reads the cell once per pass ([`Snap::read`]) and writes values that all come from
//!   that one read, two of them as single 64-bit registry values, so a reader gets count and time
//!   in ONE transaction ([`pack_pair`]): `VsSnapA` = ticks and the time of the tick that made
//!   that count, `VsSnapB` = slots and the same time;
//! * a rate from two such values is [`pair_rate_mhz`]: the count delta over the delta of the
//!   ticks' own times, exact to a millisecond over any window.
//!
//! `VsSnapB / VsSnapA` read together tell what the heartbeat really does: `slots` advance at the
//! nominal rate whatever the callbacks do (240.0 a second at 240 Hz, minus the 0.0008 % of the
//! integer 41 667 period), `ticks` advance at the nominal rate minus the slots dropped.

use core::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

use crate::vsync_rate::ms_from_100ns;

/// The cell's fields, by index.
pub const TICKS: usize = 0;
/// Interrupt time (100 ns) of the tick that made [`TICKS`].
pub const TIME: usize = 1;
/// Period slots the chain has moved over (`VsSlotN`): one per tick on time, more after a late one.
pub const SLOTS: usize = 2;
/// Slots no tick served because the callback ran a period or more late (`VsSkipN`).
pub const SKIPPED: usize = 3;
/// Ticks that served a missed slot immediately (`VsCatchN`, `VsCatchUp`).
pub const CAUGHT: usize = 4;
/// Number of fields.
pub const FIELDS: usize = 5;

/// One sample: the five fields of the cell, taken together.
pub type Sample = [u64; FIELDS];

/// A seqlock cell for one writer (the tick) and any number of readers (the mirror, the worker).
///
/// The writer never blocks and never waits for a reader; a reader retries while a write is in
/// flight, a bounded number of times, and then reports a miss (the caller keeps its last good
/// sample and counts the miss). Everything is `Relaxed` apart from the fences that order the
/// field accesses against the sequence word (the standard seqlock, Boehm 2012).
pub struct Snap {
    seq: AtomicU32,
    fields: [AtomicU64; FIELDS],
}

#[allow(clippy::declare_interior_mutable_const)]
const Z64: AtomicU64 = AtomicU64::new(0);

impl Snap {
    pub const fn new() -> Self {
        Snap {
            seq: AtomicU32::new(0),
            fields: [Z64; FIELDS],
        }
    }

    /// Store a sample. `false` when another writer is inside (a second concurrent writer is a bug
    /// of the caller, but it must not corrupt the cell: the update is dropped instead). Wait-free.
    pub fn write(&self, v: &Sample) -> bool {
        let s = self.seq.load(Ordering::Relaxed);
        if s & 1 != 0 {
            return false;
        }
        if self
            .seq
            .compare_exchange(s, s.wrapping_add(1), Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            return false;
        }
        fence(Ordering::Release);
        for (f, x) in self.fields.iter().zip(v.iter()) {
            f.store(*x, Ordering::Relaxed);
        }
        self.seq.store(s.wrapping_add(2), Ordering::Release);
        true
    }

    /// The sample, or `None` after `tries` attempts that each met a write in flight.
    pub fn read_tries(&self, tries: u32) -> Option<Sample> {
        let mut n = 0;
        while n < tries {
            n += 1;
            let before = self.seq.load(Ordering::Acquire);
            if before & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let mut out = [0u64; FIELDS];
            for (o, f) in out.iter_mut().zip(self.fields.iter()) {
                *o = f.load(Ordering::Relaxed);
            }
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == before {
                return Some(out);
            }
        }
        None
    }

    /// [`Self::read_tries`] with the default bound (a write is a handful of stores: a few
    /// attempts always suffice unless the writer is being preempted inside it, which at DISPATCH
    /// on another processor it cannot be for long).
    pub fn read(&self) -> Option<Sample> {
        self.read_tries(64)
    }

    /// Zero the cell (StartDevice, PASSIVE; the tick is not running then, or loses its update).
    pub fn reset(&self) {
        let _ = self.write(&[0; FIELDS]);
    }
}

impl Default for Snap {
    fn default() -> Self {
        Self::new()
    }
}

/// A count and the time it was made at, as one 64-bit registry value: the count's low 32 bits in
/// the high half, the interrupt time in milliseconds (low 32 bits, [`ms_from_100ns`]) in the low
/// half. Both halves are from the same instant, and a reader sees both or neither.
pub const fn pack_pair(count: u64, time_100ns: u64) -> u64 {
    ((count as u32 as u64) << 32) | ms_from_100ns(time_100ns) as u64
}

/// The count half of a packed pair.
pub const fn pair_count(pair: u64) -> u32 {
    (pair >> 32) as u32
}

/// The time half (milliseconds, wrapping) of a packed pair.
pub const fn pair_ms(pair: u64) -> u32 {
    pair as u32
}

/// The rate in millihertz between two packed pairs, `earlier` then `later`: the count delta over
/// the delta of the times the counts were made at. `None` when no time passed. One wrap of either
/// half between the two is handled (wrapping differences), the same rule as `vsync_rate::rate_mhz`.
pub const fn pair_rate_mhz(earlier: u64, later: u64) -> Option<u64> {
    crate::vsync_rate::rate_mhz(
        pair_count(earlier),
        pair_count(later),
        pair_ms(earlier),
        pair_ms(later),
    )
}

/// The share of slots that a tick served, in permille, from two samples (`earlier`, `later`):
/// 1000 when no slot was dropped, `None` when no slot passed. Catch-up ticks serve a slot AFTER
/// it was due, so they count as served here (the slot moved by one tick), which is the point.
pub const fn served_permille(earlier: &Sample, later: &Sample) -> Option<u64> {
    let slots = later[SLOTS].wrapping_sub(earlier[SLOTS]);
    if slots == 0 {
        return None;
    }
    let dropped = later[SKIPPED].wrapping_sub(earlier[SKIPPED]);
    let served = slots.saturating_sub(dropped);
    Some(served * 1000 / slots)
}

/// `VsCatchUp` (default 0): 1 serves ONE missed slot with an immediate extra tick when the
/// callback ran between one and 1.5 periods after its own deadline (`vsync_deadline::advance`);
/// 0 drops the missed slot as the heartbeat always did. Anything non-zero is 1.
pub const fn clamp_catch_up(v: u32) -> u32 {
    if v != 0 {
        1
    } else {
        0
    }
}

/// The tick callback's contribution to the cell: the sample after one more tick.
///
/// `previous` is the sample as the writer last stored it, `time_100ns` the interrupt time the
/// callback ran at, and `slots` / `skipped` / `caught_up` what `vsync_deadline::advance` decided.
pub const fn after_tick(
    previous: &Sample,
    time_100ns: u64,
    slots: u64,
    skipped: u64,
    caught_up: bool,
) -> Sample {
    [
        previous[TICKS].wrapping_add(1),
        time_100ns,
        previous[SLOTS].wrapping_add(slots),
        previous[SKIPPED].wrapping_add(skipped),
        previous[CAUGHT].wrapping_add(caught_up as u64),
    ]
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::vsync_deadline::{advance, period_100ns};

    #[test]
    fn the_cell_returns_what_was_written() {
        let c = Snap::new();
        assert_eq!(c.read(), Some([0; FIELDS]));
        assert!(c.write(&[1, 2, 3, 4, 5]));
        assert_eq!(c.read(), Some([1, 2, 3, 4, 5]));
        assert!(c.write(&[6, 7, 8, 9, 10]));
        assert_eq!(c.read(), Some([6, 7, 8, 9, 10]));
        c.reset();
        assert_eq!(c.read(), Some([0; FIELDS]));
    }

    #[test]
    fn a_read_during_a_write_misses_and_a_second_writer_is_refused() {
        let c = Snap::new();
        // Simulate a write in flight: the sequence word is odd.
        c.seq.store(1, Ordering::Relaxed);
        assert_eq!(c.read_tries(5), None);
        assert!(!c.write(&[1; FIELDS]), "no second writer inside");
        c.seq.store(2, Ordering::Relaxed);
        assert_eq!(c.read_tries(1), Some([0; FIELDS]));
        assert!(c.write(&[1; FIELDS]));
    }

    #[test]
    fn the_sequence_word_wraps_cleanly() {
        let c = Snap::new();
        c.seq.store(u32::MAX - 1, Ordering::Relaxed); // even, the next write ends at 0
        assert!(c.write(&[9; FIELDS]));
        assert_eq!(c.seq.load(Ordering::Relaxed), 0);
        assert_eq!(c.read(), Some([9; FIELDS]));
    }

    #[test]
    fn a_reader_never_sees_a_torn_sample() {
        // The writer keeps every field equal to a running number; a reader that ever saw two
        // different values in one sample read a torn cell.
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;
        let c = Arc::new(Snap::new());
        let stop = Arc::new(AtomicBool::new(false));
        let w = {
            let (c, stop) = (c.clone(), stop.clone());
            std::thread::spawn(move || {
                let mut n = 1u64;
                while !stop.load(Ordering::Relaxed) {
                    assert!(c.write(&[n; FIELDS]));
                    n += 1;
                }
                n
            })
        };
        let mut seen = 0u64;
        let mut last = 0u64;
        for _ in 0..200_000 {
            if let Some(s) = c.read() {
                assert!(s.iter().all(|x| *x == s[0]), "torn: {s:?}");
                assert!(s[0] >= last, "went back: {} after {last}", s[0]);
                last = s[0];
                seen += 1;
            }
        }
        stop.store(true, Ordering::Relaxed);
        let written = w.join().unwrap();
        assert!(seen > 1000 && written > 1, "{seen} {written}");
    }

    #[test]
    fn pairs_pack_count_and_time_in_one_word() {
        let p = pack_pair(0x1_2345_6789, 123_456 * 10_000 + 77);
        assert_eq!(pair_count(p), 0x2345_6789);
        assert_eq!(pair_ms(p), 123_456);
        // the time wraps like every other millisecond value
        let p = pack_pair(1, ((1u64 << 32) + 5) * 10_000);
        assert_eq!(pair_ms(p), 5);
    }

    #[test]
    fn two_pairs_give_the_exact_rate() {
        // An ideal 240 Hz heartbeat: tick n at n * 41_667 (100 ns), sampled at two instants that
        // are NOT a whole number of seconds apart and not on tick boundaries.
        let p = period_100ns(240_000);
        let tick_at = |n: u64| 1_000_000 + n * p;
        let a = pack_pair(500, tick_at(500));
        let b = pack_pair(500 + 2432, tick_at(500 + 2432));
        let rate = pair_rate_mhz(a, b).unwrap();
        // 2432 ticks over 2432 periods of 4.1667 ms: 240 Hz to the millisecond of each end
        assert!((239_950..=240_050).contains(&rate), "{rate}");
        // a reader that divided by the interval it ASKED for (10 s) instead would be 5 % out
        assert_eq!(pair_rate_mhz(a, a), None);
        // one wrap of the count half
        let a = pack_pair(0xFFFF_FFF0, tick_at(10));
        let b = pack_pair(0xFFFF_FFF0 + 240, tick_at(10 + 240));
        let rate = pair_rate_mhz(a, b).unwrap();
        assert!((239_000..=241_000).contains(&rate), "{rate}");
    }

    #[test]
    fn slots_advance_at_the_nominal_rate_whatever_the_callbacks_do() {
        // A chain where every 7th callback runs 1.3 periods late and the rest 0.3 ms late; drive
        // `advance` and `after_tick` the way the tick does and read the cell like the mirror.
        let p = period_100ns(240_000);
        for catch_up in [false, true] {
            let cell = Snap::new();
            let start = 10_000_000u64;
            let mut deadline = start;
            let mut fire = deadline + 3_000;
            let mut cur: Sample = [0; FIELDS];
            let mut n = 0u64;
            let mut first = None;
            let end = start + 20 * 10_000_000;
            while fire < end {
                let a = advance(deadline, fire, p, catch_up).unwrap();
                cur = after_tick(&cur, fire, a.slots, a.skipped, a.caught_up);
                assert!(cell.write(&cur));
                if first.is_none() {
                    first = Some(cell.read().unwrap());
                }
                deadline = a.deadline;
                n += 1;
                fire = if a.caught_up {
                    fire + 1
                } else if n % 7 == 0 {
                    a.deadline + p + p * 3 / 10
                } else {
                    a.deadline + 3_000
                };
            }
            let last = cell.read().unwrap();
            let first = first.unwrap();
            // slots: the grid, exactly (20 s of 41_667 units, give or take the slot in flight)
            let slots = last[SLOTS] - first[SLOTS];
            let elapsed = last[TIME] - first[TIME];
            let slot_rate = slots * 10_000_000_000 / elapsed;
            assert!((239_900..=240_100).contains(&slot_rate), "catch_up={catch_up} {slot_rate}");
            // ticks: nominal minus what was dropped, and `served_permille` says how much
            let ticks = last[TICKS] - first[TICKS];
            let dropped = last[SKIPPED] - first[SKIPPED];
            // every slot is either served by a tick or dropped: nothing else moves the grid
            assert_eq!(slots, ticks + dropped, "{slots} {ticks} {dropped}");
            let served = served_permille(&first, &last).unwrap();
            if catch_up {
                assert_eq!(dropped, 0);
                assert_eq!(served, 1000);
                assert!(last[CAUGHT] > 0);
            } else {
                assert!(dropped > 0 && served < 1000);
                assert_eq!(last[CAUGHT], 0);
            }
        }
    }

    #[test]
    fn the_catch_up_knob_is_a_switch() {
        assert_eq!(clamp_catch_up(0), 0);
        assert_eq!(clamp_catch_up(1), 1);
        assert_eq!(clamp_catch_up(7), 1);
        assert_eq!(clamp_catch_up(u32::MAX), 1);
    }

    #[test]
    fn served_share_needs_a_slot() {
        let a = [0; FIELDS];
        assert_eq!(served_permille(&a, &a), None);
        let b = [10, 0, 10, 1, 0];
        assert_eq!(served_permille(&a, &b), Some(900));
    }
}
