//! The copy-engine shadow mode (`RmCopyEngine` = 3, milestone M3c-1): the pure half. The I/O
//! half is `kmd_render/src/virtio/rm_client/ce_shadow.rs`; the procedure and the meaning of every
//! outcome are in `docs/rm-copy-engine-present.md` 14.
//!
//! For a bounded sample of real Presents (one in [`every_in_force`], at most one in flight), the
//! HPD worker copies the Present's source image (the `'HEF3'` record's, dup'd and mapped by
//! `ce_dup`) with the KMD's copy-engine channel into a scratch buffer laid out like the Present's
//! destination, AFTER the production (Venus) copy of that Present completed, and compares the
//! scratch with the destination's system pages. Nothing the Present path does changes.
//!
//! What is here:
//!
//! * [`capture`]: what a Present in shadow mode does with the sample slot (take it, re-target the
//!   pending sample to this newer frame of the same destination, or leave it).
//! * [`Skip`]: why a sampled Present was not compared, and which of those are strikes; three
//!   strikes ([`MAX_STRIKES`]) disable the shadow mode for the transport generation.
//! * [`covers`] / [`span_pieces`]: the destination's lease runs (allocation offset, length)
//!   cover the surface, and how one row is gathered from them.
//! * [`Tally`] / [`Verdict`]: the comparison, pixel by pixel, with the destination format's
//!   undefined byte masked ([`dst_mask`]): equal pixels, pixels equal after an R/B exchange of the
//!   scratch ([`swap_rb`]), the first differing pixel; the percentage bins of `CeShadowP*` and the
//!   swapped-channel pattern of `CeShadowSwp`.
//! * [`COUNTERS`] and [`WRITERS`].
//!
//! Nothing here does I/O, reads a clock or takes a lock.

use crate::foreign_resource::{FOURCC_XBGR8888, FOURCC_XRGB8888};

/// `CeShadowEvery` (service-key REG_DWORD, read at StartDevice with `RmCopyEngine` = 3): one in
/// how many Presents is shadowed. 0 or unset is the default.
pub const EVERY_KNOB: &str = "CeShadowEvery";
pub const EVERY_DEFAULT: u32 = 64;
/// The densest sampling allowed (every Present), and the sparsest.
pub const EVERY_MIN: u32 = 1;
pub const EVERY_MAX: u32 = 1 << 16;

/// The sampling period in force.
pub const fn every_in_force(knob: u32) -> u32 {
    if knob == 0 {
        EVERY_DEFAULT
    } else if knob > EVERY_MAX {
        EVERY_MAX
    } else {
        knob
    }
}

/// How long the copy may take after its submission. The producer's value was reached before the
/// production copy ran, so the acquire is satisfied at once and the copy is about 0.2 ms; a copy
/// still not done after this is stuck (most likely on an acquire of a wrong address or value).
pub const COPY_DEADLINE_MS: u64 = 100;
/// How long a sample may wait for its destination to settle (the production copy and its mirror
/// done) before it is dropped (`Skip::Expired`).
pub const EXPIRE_MS: u64 = 500;
/// Spin this long on the completion before sleeping in ticks (a 1600x900 copy is ~0.2 ms).
pub const SPIN_US: u64 = 2_000;
/// Strikes after which the shadow mode stops for the transport generation.
pub const MAX_STRIKES: u32 = 3;
/// The scratch buffer's largest size: its VA window.
pub const SCRATCH_MAX: u64 = crate::rm_ce_channel::VA_WINDOW;
/// The word the scratch is filled with before each copy: a copy that wrote nothing never
/// compares equal by accident with a stale scratch.
pub const POISON: u32 = 0x5a5a_a5a5;

/// What a Present in shadow mode does with the sample slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Capture {
    /// Nothing (not sampled, or another sample is pending or in flight).
    Ignore,
    /// Take this Present as the sample.
    Take,
    /// A sample of the same destination is pending: this newer frame replaces it (its record and
    /// boundary), so a destination that settles always holds the sample's frame or a newer one.
    Retarget,
}

/// `seen`: Presents seen in shadow mode including this one (1-based); `pending`: the destination
/// of the pending sample; `in_flight`: a sample is being copied or compared.
pub const fn capture(seen: u64, every: u32, pending: Option<u32>, in_flight: bool, dst: u32) -> Capture {
    if let Some(p) = pending {
        return if p == dst { Capture::Retarget } else { Capture::Ignore };
    }
    if in_flight || every == 0 {
        return Capture::Ignore;
    }
    if seen % every as u64 == 0 {
        Capture::Take
    } else {
        Capture::Ignore
    }
}

/// Why a sample was not compared. `code` is `CeShadowWhy`, `bit` the `CeShadowMask` bit; codes
/// are appended, never renumbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Skip {
    /// The Present carried no record for its boundary (the UMD sent the 48/96-byte tail, or the
    /// fence was not attached).
    NoRecord = 1,
    /// The destination has no full, valid system backing to compare (not leased, partly leased,
    /// its system copy marked stale by `BltNoMirror`, or gone).
    NoDestination = 2,
    /// The channel's I/O was busy (StopDevice) when the sample was taken out.
    Busy = 3,
    /// `NV_ESC_RM_DUP_OBJECT` of the record's semaphore or image was refused (`CeDupStat`).
    DupRefused = 4,
    /// A GPU mapping of a dup'd object was refused (`CeMapStat`).
    MapFailed = 5,
    /// The copy did not complete within [`COPY_DEADLINE_MS`]: the producer's value was not reached
    /// at the address the KMD acquires on (`CeShadowSem` holds the value read there), or the
    /// channel hung. The channel is torn down.
    NotReached = 6,
    /// The record or the destination is not a pair this copy does: format (`remap_for`), layout
    /// (`source_plan`), extent, or size.
    Unsupported = 7,
    /// The channel is not up (bring-up failed, cooling down), its error notifier is set, the ring
    /// is full, or the push did not build.
    Channel = 8,
    /// The destination did not settle within [`EXPIRE_MS`].
    Expired = 9,
    /// Struck out for this generation.
    Disabled = 10,
    /// The scratch buffer's RM memory, CPU view or GPU mapping failed.
    Scratch = 11,
}

impl Skip {
    pub const fn code(self) -> u32 {
        self as u32
    }

    pub const fn bit(self) -> u32 {
        1 << (self.code() - 1)
    }

    /// A failure of the shadow machinery itself (a strike), not a circumstance of the frame.
    pub const fn strikes(self) -> bool {
        matches!(
            self,
            Skip::DupRefused | Skip::MapFailed | Skip::NotReached | Skip::Channel | Skip::Scratch
        )
    }
}

/// Whether the lease runs `(allocation offset, length)`, sorted by offset as the backing table
/// keeps them, cover `[0, need)` without a gap.
pub fn covers(runs: &[(u64, u64)], need: u64) -> bool {
    let mut end = 0u64;
    for &(off, len) in runs {
        if off > end {
            return false;
        }
        let Some(e) = off.checked_add(len) else {
            return false;
        };
        end = end.max(e);
        if end >= need {
            return true;
        }
    }
    need == 0
}

/// The pieces of `[start, start + len)` in `runs` (sorted): `f(run index, offset in the run, offset
/// in the span, bytes)` once per piece, in order. `false` when a byte is not covered.
pub fn span_pieces(
    runs: &[(u64, u64)],
    start: u64,
    len: u64,
    mut f: impl FnMut(usize, u64, u64, u64),
) -> bool {
    let Some(end) = start.checked_add(len) else {
        return false;
    };
    let mut at = start;
    for (i, &(off, size)) in runs.iter().enumerate() {
        if at >= end {
            break;
        }
        let Some(run_end) = off.checked_add(size) else {
            return false;
        };
        if run_end <= at {
            continue;
        }
        if off > at {
            return false;
        }
        let n = run_end.min(end) - at;
        f(i, at - off, at - start, n);
        at += n;
    }
    at >= end
}

/// The scratch with bytes 0 and 2 of every pixel exchanged (RGBA <-> BGRA).
pub const fn swap_rb(p: u32) -> u32 {
    (p & 0xff00_ff00) | ((p >> 16) & 0xff) | ((p & 0xff) << 16)
}

/// The bytes that are defined in a destination of `dst_fourcc`: the X formats' fourth byte is
/// undefined (neither copy promises it).
pub const fn dst_mask(dst_fourcc: u32) -> u32 {
    match dst_fourcc {
        FOURCC_XRGB8888 | FOURCC_XBGR8888 => 0x00ff_ffff,
        _ => 0xffff_ffff,
    }
}

/// The comparison of a scratch with a destination, row by row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub total: u64,
    pub equal: u64,
    /// Pixels equal after [`swap_rb`] of the scratch.
    pub swapped: u64,
    /// The first differing pixel `(row, x)`.
    pub first_diff: Option<(u32, u32)>,
}

impl Tally {
    /// Compare row `y`: `scratch` and `dst` the same number of pixels.
    pub fn row(&mut self, y: u32, scratch: &[u32], dst: &[u32], mask: u32) {
        for (x, (&s, &d)) in scratch.iter().zip(dst.iter()).enumerate() {
            self.total += 1;
            let d = d & mask;
            if s & mask == d {
                self.equal += 1;
            } else if self.first_diff.is_none() {
                self.first_diff = Some((y, x as u32));
            }
            if swap_rb(s) & mask == d {
                self.swapped += 1;
            }
        }
    }

    pub fn verdict(&self) -> Verdict {
        let pct = |n: u64| if self.total == 0 { 0 } else { (n * 100 / self.total) as u32 };
        let equal_pct = pct(self.equal);
        let swapped_pct = pct(self.swapped);
        let all = self.total != 0 && self.equal == self.total;
        let bin = if all {
            Bin::All
        } else if equal_pct >= 99 {
            Bin::P99
        } else if equal_pct >= 90 {
            Bin::P90
        } else {
            Bin::Low
        };
        Verdict {
            all_equal: all,
            equal_pct,
            swapped_pct,
            bin,
            // The scratch is the destination with R and B exchanged: the remap ran in the wrong
            // direction (or ran where none was due, or did not run where one was).
            swap_pattern: swapped_pct >= 99 && self.swapped > self.equal,
            row_word: match self.first_diff {
                Some((y, x)) => ((y & 0xffff) << 16) | (x & 0xffff),
                None => 0,
            },
        }
    }
}

/// The four bins of `CeShadowP100` / `P99` / `P90` / `PLow`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bin {
    All,
    P99,
    P90,
    Low,
}

/// What one comparison says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Verdict {
    pub all_equal: bool,
    /// `CeShadowPct`: percent of equal pixels, rounded down (99 when one pixel of 1.44 M differs).
    pub equal_pct: u32,
    /// `CeShadowSwPct`.
    pub swapped_pct: u32,
    pub bin: Bin,
    pub swap_pattern: bool,
    /// `CeShadowRow`: `row << 16 | x` of the first differing pixel, 0 when all are equal.
    pub row_word: u32,
}

/// The counters the shadow mode writes, all in `kmd_render/src/virtio/rm_client/ce_shadow.rs`.
/// At most 14 characters, prefix `Ce`, unique across `kmd_render` and `kmd_logic`.
pub const COUNTERS: &[&str] = &[
    // The sampling period in force; Presents seen in shadow mode.
    "CeShadowEach",
    "CeShadowSeen",
    // Shadow copies attempted (a sample taken out by the worker), compared all equal, compared
    // with a difference; the percent of equal pixels of the last compare and its first differing
    // pixel (`row << 16 | x`).
    "CeShadowN",
    "CeShadowOk",
    "CeShadowBad",
    "CeShadowPct",
    "CeShadowRow",
    // Samples not compared (`Skip`): how many, the last reason, every reason seen.
    "CeShadowSkip",
    "CeShadowWhy",
    "CeShadowMask",
    // The bins of the percent of equal pixels: 100, >= 99, >= 90, < 90.
    "CeShadowP100",
    "CeShadowP99",
    "CeShadowP90",
    "CeShadowPLow",
    // Compares whose scratch is the destination with R and B exchanged; the percent of pixels
    // equal after that exchange in the last compare.
    "CeShadowSwp",
    "CeShadowSwPct",
    // Compares during which the destination was presented again or stopped being settled (the
    // result mixes two frames).
    "CeShadowRace",
    // Microseconds of the last attempt: the copy (kick to completion seen), the dup + map (0 when
    // cached), the compare.
    "CeShadowUs",
    "CeShadowDupUs",
    "CeShadowCmpUs",
    // The producer's semaphore value (low 32 bits) read through the KMD's mapping when a copy
    // did not complete; the strikes so far.
    "CeShadowSem",
    "CeShadowStrk",
];

/// The files that write [`COUNTERS`] (relative to `kmd_render/src`).
pub const WRITERS: [&str; 1] = ["virtio/rm_client/ce_shadow.rs"];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use crate::ce_dup::scan;
    use crate::foreign_resource::{FOURCC_ABGR8888, FOURCC_ARGB8888};

    #[test]
    fn the_sampling_period() {
        assert_eq!(every_in_force(0), 64);
        assert_eq!(every_in_force(1), 1);
        assert_eq!(every_in_force(7), 7);
        assert_eq!(every_in_force(u32::MAX), EVERY_MAX);
        assert_eq!(EVERY_KNOB.len(), 13);
    }

    #[test]
    fn one_in_n_one_at_a_time_and_a_newer_frame_retargets() {
        let take: std::vec::Vec<u64> =
            (1..=200).filter(|&n| capture(n, 64, None, false, 7) == Capture::Take).collect();
        assert_eq!(take, [64, 128, 192]);
        assert_eq!(capture(64, 64, None, true, 7), Capture::Ignore);
        assert_eq!(capture(64, 64, Some(9), false, 7), Capture::Ignore);
        assert_eq!(capture(65, 64, Some(7), false, 7), Capture::Retarget);
        assert_eq!(capture(1, 1, None, false, 7), Capture::Take);
    }

    #[test]
    fn skip_codes_and_strikes() {
        let all = [
            Skip::NoRecord, Skip::NoDestination, Skip::Busy, Skip::DupRefused, Skip::MapFailed,
            Skip::NotReached, Skip::Unsupported, Skip::Channel, Skip::Expired, Skip::Disabled,
            Skip::Scratch,
        ];
        for (i, s) in all.iter().enumerate() {
            assert_eq!(s.code(), i as u32 + 1);
            assert_eq!(s.bit(), 1 << i);
        }
        let strikes: std::vec::Vec<Skip> = all.iter().copied().filter(|s| s.strikes()).collect();
        assert_eq!(
            strikes,
            [Skip::DupRefused, Skip::MapFailed, Skip::NotReached, Skip::Channel, Skip::Scratch]
        );
    }

    #[test]
    fn coverage_and_row_pieces() {
        let runs = [(0u64, 4096u64), (4096, 8192), (12288, 4096)];
        assert!(covers(&runs, 16384));
        assert!(!covers(&runs, 16385));
        assert!(!covers(&[(0, 4096), (8192, 4096)], 12288), "a gap");
        assert!(!covers(&[(4096, 4096)], 4096), "not from 0");
        assert!(covers(&[], 0));
        let mut seen = std::vec::Vec::new();
        assert!(span_pieces(&runs, 4000, 200, |i, o, at, n| seen.push((i, o, at, n))));
        assert_eq!(seen, [(0, 4000, 0, 96), (1, 0, 96, 104)]);
        seen.clear();
        assert!(span_pieces(&runs, 12000, 1000, |i, o, at, n| seen.push((i, o, at, n))));
        assert_eq!(seen, [(1, 7904, 0, 288), (2, 0, 288, 712)]);
        assert!(!span_pieces(&runs, 16000, 1000, |_, _, _, _| {}));
        assert!(!span_pieces(&[(0, 100), (200, 100)], 50, 200, |_, _, _, _| {}));
    }

    #[test]
    fn the_comparison_and_its_bins() {
        let row: std::vec::Vec<u32> = (0..100u32).map(|i| 0xff00_0000 | i * 0x01_0203).collect();
        let mut t = Tally::default();
        t.row(0, &row, &row, u32::MAX);
        let v = t.verdict();
        assert!(v.all_equal);
        assert_eq!((v.equal_pct, v.bin, v.row_word, v.swap_pattern), (100, Bin::All, 0, false));

        // One pixel differs in 100: 99 %, its position.
        let mut other = row.clone();
        other[37] ^= 1;
        let mut t = Tally::default();
        t.row(5, &row, &other, u32::MAX);
        let v = t.verdict();
        assert_eq!((v.equal_pct, v.bin, v.row_word), (99, Bin::P99, (5 << 16) | 37));

        // The destination is the scratch with R and B exchanged: the swap pattern.
        let swapped: std::vec::Vec<u32> = row.iter().map(|&p| swap_rb(p)).collect();
        let mut t = Tally::default();
        t.row(0, &row, &swapped, u32::MAX);
        let v = t.verdict();
        assert!(v.swap_pattern && !v.all_equal);
        assert_eq!(v.swapped_pct, 100);
        assert_eq!(v.bin, Bin::Low);

        // An X destination ignores the fourth byte.
        let x: std::vec::Vec<u32> = row.iter().map(|&p| p & 0x00ff_ffff).collect();
        let mut t = Tally::default();
        t.row(0, &row, &x, dst_mask(FOURCC_XRGB8888));
        assert!(t.verdict().all_equal);
        assert_eq!(dst_mask(FOURCC_ARGB8888), u32::MAX);
        assert_eq!(dst_mask(FOURCC_ABGR8888), u32::MAX);

        // 85 of 100: the low bin.
        let mut t = Tally::default();
        let mut bad = row.clone();
        for p in bad.iter_mut().take(15) {
            *p ^= 0x10;
        }
        t.row(0, &row, &bad, u32::MAX);
        assert_eq!((t.verdict().equal_pct, t.verdict().bin), (85, Bin::Low));
        assert_eq!(Tally::default().verdict().bin, Bin::Low);
        assert_eq!(swap_rb(0x11223344), 0x11443322);
    }

    #[test]
    fn counter_names_fit_and_are_unique() {
        scan::names_fit(
            COUNTERS,
            &[
                crate::ce_present::COUNTERS,
                crate::rm_ce_channel::COUNTERS,
                crate::ce_record::COUNTERS,
                crate::ce_dup::COUNTERS,
                crate::blt_async::COUNTERS,
                crate::guest_blob::COUNTERS,
                &crate::onscanout::COUNTERS,
            ],
        );
        assert!(!COUNTERS.contains(&EVERY_KNOB), "the knob and a counter would share a value");
    }

    #[test]
    fn the_counters_the_driver_writes_are_exactly_the_ones_listed() {
        scan::exact_list(COUNTERS, &WRITERS, "CeShadow");
    }
}
