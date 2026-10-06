//! The "already on scanout" present tag (`HOSC`): the pure half. Parse the tag out of the tail of a
//! `HERF` Render command, and decide whether the KMD may complete the Present that follows with
//! no copy. No memory, no transport, no clock: the I/O half is `kmd_render/src/ddi/onscanout.rs`.
//! Wire layout: `protocol/src/onscanout.rs`, `protocol/include/helios_onscanout.h`. Design:
//! `docs/zero-copy-present.md`, "Already-on-scanout present tag".
//!
//! WHAT IS BEING SKIPPED. A producer that put the frame on scanout through the user foreign-scanout
//! source (`SCANOUT_SET` / `SCANOUT_PRESENT`) still presents through dxgkrnl (the D3D11 frame
//! latency wait), and dxgkrnl turns that into a whole-surface windowed `Blt` into the redirection
//! surface. While the source is live the desktop's host flush is withheld, so nothing shows that
//! surface: the copy is waste.
//!
//! WHY A LYING TAG IS HARMLESS. The skip happens only if the KMD's own state backs every part of the
//! claim ([`verify`]): a live, unlapsed USER source, minted by a device of the SAME PROCESS as the
//! presenting context (`hKmdProcess`, the token stream markers authenticate with), the tag's
//! generation is that source's, its sequence is one the source really minted and a recent one. A
//! process that cannot satisfy that gets the ordinary Blt. A process that can is the owner of the live
//! source, which is the only thing that decides what scanout 0 shows; what it can lose by lying is
//! the update of its own window's redirection surface for this frame.

use crate::present_foreign::Arm;

/// `'HOSC'` (`protocol::HELIOS_ONSCANOUT_MAGIC`; pinned by the KMD build).
pub const MAGIC: u32 = 0x4353_4F48;
/// `protocol::HELIOS_ONSCANOUT_VERSION`.
pub const VERSION: u16 = 1;
/// The tag's size.
pub const TAG_BYTES: usize = 24;
/// Where the tag sits in a `HERF` command.
pub const HERF_OFFSET: usize = 48;
/// `CommandLength` of a `HERF` command that carries the whole tag.
pub const HERF_BYTES: usize = HERF_OFFSET + TAG_BYTES;
/// `protocol::HELIOS_ONSCANOUT_MAX_LAG`: a tag more than this many `SCANOUT_PRESENT`s behind the
/// newest one of its source is stale.
pub const MAX_LAG: u64 = 256;

/// A parsed, well-formed tag: every field the wire carries that a verdict looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag {
    /// The `out_seq` of this frame's `SCANOUT_PRESENT`. Nonzero.
    pub sequence: u64,
    /// The source's `out_generation`. Nonzero.
    pub generation: u32,
    /// The Blt source's resource id, or 0 for "not stated".
    pub resource_id: u32,
}

/// Why a tag was not honoured: the value of `OsRejWhy`. Stable, read off a registry mirror.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Why {
    /// The bytes at the tag offset are nonzero and are not `HOSC`.
    BadMagic = 1,
    /// `HOSC` but fewer than [`TAG_BYTES`] bytes of command.
    Short = 2,
    /// A version this KMD does not know.
    Version = 3,
    /// A flags bit this KMD does not know.
    Flags = 4,
    /// `sequence` or `generation` is zero.
    Fields = 5,
    /// The Present is a flip, or a Blt with no allocation list (nothing to copy: the flip
    /// machinery must run as ever).
    NotBlt = 6,
    /// `ColorFill`: no source frame to claim.
    ColorFill = 7,
    /// A Blt with destination sub-rects: not a whole-frame present.
    SubRects = 8,
    /// A windowed-Blt snapshot accompanies the Present: contradictory, the snapshot owns the copy.
    Snapshot = 9,
    /// No live user source, or it has lapsed, or the tag's source never minted a frame.
    NoSource = 10,
    /// The live source is not the tag's generation.
    Generation = 11,
    /// The presenting process is not the live source's process (or one of them is unknown).
    Owner = 12,
    /// `sequence` is newer than anything the source minted.
    Ahead = 13,
    /// `sequence` is more than [`MAX_LAG`] behind the newest.
    Stale = 14,
    /// The tag names a resource id and the Present's source is another.
    Resource = 15,
    /// The Render's stash was replaced or dropped before a Present took it (an orphan).
    Orphan = 16,
    /// The Present could not take the skip (DMA or private buffer too small, patch capacity); dxgkrnl
    /// retries it without the tag and the retry is the ordinary Blt.
    Retry = 17,
}

impl Why {
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// What the tail of a `HERF` command held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parsed {
    /// No claim: the command ends before the tag, or the tag's bytes are zero.
    Absent,
    /// A well-formed claim, not yet verified.
    Tag(Tag),
    /// A claim that is malformed: counted, never stashed.
    Reject(Why),
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(a)
}

/// Parse the bytes of a `HERF` command from [`HERF_OFFSET`] to its `CommandLength` (any length;
/// only the first [`TAG_BYTES`] are looked at). Little-endian.
///
/// A tail that is empty, shorter than the magic or whose magic bytes are zero is [`Parsed::Absent`]
/// (every legacy and zero-filled command). Anything else is a claim: well-formed, or a [`Parsed::Reject`]
/// that names the first thing wrong. A short buffer is judged by the magic first, so a stray nonzero
/// word is `BadMagic` and a real `HOSC` cut short is `Short`.
pub fn parse(tail: &[u8]) -> Parsed {
    if tail.len() < 4 {
        return Parsed::Absent;
    }
    let magic = u32_at(tail, 0);
    if magic == 0 {
        return Parsed::Absent;
    }
    if magic != MAGIC {
        return Parsed::Reject(Why::BadMagic);
    }
    if tail.len() < TAG_BYTES {
        return Parsed::Reject(Why::Short);
    }
    if u16_at(tail, 4) != VERSION {
        return Parsed::Reject(Why::Version);
    }
    if u16_at(tail, 6) != 0 {
        return Parsed::Reject(Why::Flags);
    }
    let sequence = u64_at(tail, 8);
    let generation = u32_at(tail, 16);
    let resource_id = u32_at(tail, 20);
    if sequence == 0 || generation == 0 {
        return Parsed::Reject(Why::Fields);
    }
    Parsed::Tag(Tag {
        sequence,
        generation,
        resource_id,
    })
}

/// The live user source and what its minter has done, as the KMD's own state says it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Live {
    /// The source's generation.
    pub generation: u32,
    /// `hKmdProcess` of the device that minted the source's frames. 0 = unknown.
    pub process: u64,
    /// The newest `SCANOUT_PRESENT` sequence minted for that generation. 0 = none yet.
    pub newest_sequence: u64,
}

/// Everything about the Present a verdict may read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    pub arm: Arm,
    /// The Present carries no allocation list (a Blt with nothing named: nothing to copy, nothing
    /// to claim).
    pub no_allocations: bool,
    /// `DXGK_PRESENTFLAGS.ColorFill`.
    pub color_fill: bool,
    /// `SubRectCnt != 0` with a sub-rect array: a partial Blt.
    pub sub_rects: bool,
    /// A snapshot descriptor was stashed for this Present.
    pub snapshot: bool,
    /// `hKmdProcess` of the presenting context's device. 0 = unknown.
    pub presenter_process: u64,
    /// The Present's source resource id, 0 if the source did not resolve.
    pub source_resource_id: u32,
    /// The live user source, `None` if there is none (or it lapsed).
    pub live: Option<Live>,
}

/// The decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Complete the Present with no copy and no host call.
    Skip,
    /// Do the ordinary Present; count why.
    Reject(Why),
}

/// May the KMD skip the copy of this Present, given this tag?
///
/// Every check is on the KMD's own facts, none on the UMD's say-so; the order is the order of
/// `OsRejWhy` meaning "the first reason" and is part of the contract (the tests pin it).
pub fn verify(tag: &Tag, facts: &Facts) -> Verdict {
    if facts.arm != Arm::Blt || facts.no_allocations {
        return Verdict::Reject(Why::NotBlt);
    }
    if facts.color_fill {
        return Verdict::Reject(Why::ColorFill);
    }
    if facts.sub_rects {
        return Verdict::Reject(Why::SubRects);
    }
    if facts.snapshot {
        return Verdict::Reject(Why::Snapshot);
    }
    let Some(live) = facts.live else {
        return Verdict::Reject(Why::NoSource);
    };
    if live.generation != tag.generation {
        return Verdict::Reject(Why::Generation);
    }
    if facts.presenter_process == 0 || live.process == 0 || facts.presenter_process != live.process
    {
        return Verdict::Reject(Why::Owner);
    }
    if live.newest_sequence == 0 {
        return Verdict::Reject(Why::NoSource);
    }
    if tag.sequence > live.newest_sequence {
        return Verdict::Reject(Why::Ahead);
    }
    if live.newest_sequence - tag.sequence > MAX_LAG {
        return Verdict::Reject(Why::Stale);
    }
    if tag.resource_id != 0 && tag.resource_id != facts.source_resource_id {
        return Verdict::Reject(Why::Resource);
    }
    Verdict::Skip
}

/// The newest-frame record the I/O half keeps for the live user source: one generation, who minted
/// it, the newest sequence. Kept here so its update rule is host-tested.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Shown {
    pub generation: u32,
    pub process: u64,
    pub newest_sequence: u64,
}

impl Shown {
    pub const fn new() -> Self {
        Self {
            generation: 0,
            process: 0,
            newest_sequence: 0,
        }
    }

    /// A `SCANOUT_PRESENT` minted `sequence` for `generation`, from a device of `process`.
    /// A new generation replaces the record; within a generation the newest sequence only grows
    /// (a fenced present can be minted out of order with a direct one only by one lock hold, never
    /// backwards, but the rule does not rely on it) and the process is the latest minter's.
    pub fn minted(&mut self, generation: u32, process: u64, sequence: u64) {
        if generation == 0 {
            return;
        }
        if self.generation != generation {
            *self = Self {
                generation,
                process,
                newest_sequence: sequence,
            };
            return;
        }
        self.process = process;
        if sequence > self.newest_sequence {
            self.newest_sequence = sequence;
        }
    }

    /// The record as a [`Live`] for the live source's `generation`: `None` when the record is for
    /// another generation (nothing minted for this one yet).
    pub fn live_for(&self, generation: u32) -> Option<Live> {
        (generation != 0 && self.generation == generation).then_some(Live {
            generation,
            process: self.process,
            newest_sequence: self.newest_sequence,
        })
    }
}

/// The bytes of the copy a skip avoided: `pitch * height` when the pitch covers the width at 4
/// bytes per pixel, else `width * height * 4`. Saturating; 0 for an unresolved source.
pub fn extent_bytes(width: u32, height: u32, pitch: u32) -> u64 {
    let row = if u64::from(pitch) >= u64::from(width) * 4 {
        u64::from(pitch)
    } else {
        u64::from(width) * 4
    };
    row.saturating_mul(u64::from(height))
}

/// The service-key counter names the I/O half writes (`kmd_render/src/ddi/onscanout.rs`): at most
/// 14 characters (`record_named_bytes` clamps there), none equal to any other counter in
/// `kmd_render` or `kmd_logic` (host-tested by scanning both trees).
///
/// * `OsTag`: presents whose `HERF` carried a claim (a tag, well-formed or not).
///   `OsTag = OsSkip + OsRej` once nothing is in flight.
/// * `OsSkip`: completed with no copy and no host call.
/// * `OsRej`: claims not honoured; `OsRejWhy` the last reason ([`Why::code`]); `OsWhyMask` every
///   reason seen since the start of the generation, bit `code - 1`.
/// * `OsBytes`: MiB of copy avoided (the source's extent bytes, summed).
/// * `OsLast`: the last honoured sequence (low 32 bits).
pub const COUNTERS: [&str; 7] = [
    "OsTag",
    "OsSkip",
    "OsRej",
    "OsRejWhy",
    "OsWhyMask",
    "OsBytes",
    "OsLast",
];

/// How many reasons there are (codes 1 ..= `WHY_COUNT`).
pub const WHY_COUNT: usize = 17;

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn tag_bytes(
        version: u16,
        flags: u16,
        seq: u64,
        generation: u32,
        resid: u32,
    ) -> [u8; TAG_BYTES] {
        let mut b = [0u8; TAG_BYTES];
        b[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        b[4..6].copy_from_slice(&version.to_le_bytes());
        b[6..8].copy_from_slice(&flags.to_le_bytes());
        b[8..16].copy_from_slice(&seq.to_le_bytes());
        b[16..20].copy_from_slice(&generation.to_le_bytes());
        b[20..24].copy_from_slice(&resid.to_le_bytes());
        b
    }

    fn good() -> Tag {
        Tag {
            sequence: 1000,
            generation: 7,
            resource_id: 0,
        }
    }

    fn facts() -> Facts {
        Facts {
            arm: Arm::Blt,
            no_allocations: false,
            color_fill: false,
            sub_rects: false,
            snapshot: false,
            presenter_process: 0xAA,
            source_resource_id: 55,
            live: Some(Live {
                generation: 7,
                process: 0xAA,
                newest_sequence: 1000,
            }),
        }
    }

    // ---- parse -------------------------------------------------------------------------

    #[test]
    fn constants_match_the_protocol_values() {
        assert_eq!(&MAGIC.to_le_bytes(), b"HOSC");
        assert_eq!(HERF_BYTES, 72);
        assert_eq!(HERF_OFFSET, 48);
    }

    #[test]
    fn a_well_formed_tag_parses() {
        let b = tag_bytes(1, 0, 1000, 7, 0);
        assert_eq!(parse(&b), Parsed::Tag(good()));
        let b = tag_bytes(1, 0, u64::MAX, u32::MAX, 9);
        assert_eq!(
            parse(&b),
            Parsed::Tag(Tag {
                sequence: u64::MAX,
                generation: u32::MAX,
                resource_id: 9
            })
        );
    }

    #[test]
    fn trailing_bytes_after_the_tag_are_ignored() {
        let mut v = tag_bytes(1, 0, 5, 2, 0).to_vec();
        v.extend_from_slice(&[0xEE; 40]);
        assert!(matches!(parse(&v), Parsed::Tag(_)));
    }

    #[test]
    fn no_tail_a_tiny_tail_and_a_zero_tail_are_not_claims() {
        assert_eq!(parse(&[]), Parsed::Absent);
        assert_eq!(parse(&[b'H', b'O', b'S']), Parsed::Absent);
        assert_eq!(parse(&[0u8; 24]), Parsed::Absent);
        assert_eq!(parse(&[0u8; 5]), Parsed::Absent);
    }

    #[test]
    fn a_forged_magic_is_rejected() {
        let mut b = tag_bytes(1, 0, 1000, 7, 0);
        b[0] = b'X';
        assert_eq!(parse(&b), Parsed::Reject(Why::BadMagic));
        // The magic of another record (a HERF at the tag offset) is not a tag either.
        let mut b = [0u8; 24];
        b[0..4].copy_from_slice(&0x4652_4548u32.to_le_bytes());
        assert_eq!(parse(&b), Parsed::Reject(Why::BadMagic));
        // Short and forged: the magic decides first.
        assert_eq!(parse(&[1, 2, 3, 4, 5]), Parsed::Reject(Why::BadMagic));
    }

    #[test]
    fn a_short_buffer_is_rejected_at_every_length() {
        let full = tag_bytes(1, 0, 1000, 7, 0);
        for len in 4..TAG_BYTES {
            assert_eq!(parse(&full[..len]), Parsed::Reject(Why::Short), "len {len}");
        }
    }

    #[test]
    fn unknown_version_flags_and_zero_fields_are_rejected() {
        assert_eq!(
            parse(&tag_bytes(0, 0, 1000, 7, 0)),
            Parsed::Reject(Why::Version)
        );
        assert_eq!(
            parse(&tag_bytes(2, 0, 1000, 7, 0)),
            Parsed::Reject(Why::Version)
        );
        assert_eq!(
            parse(&tag_bytes(1, 1, 1000, 7, 0)),
            Parsed::Reject(Why::Flags)
        );
        assert_eq!(
            parse(&tag_bytes(1, 0x8000, 1000, 7, 0)),
            Parsed::Reject(Why::Flags)
        );
        assert_eq!(
            parse(&tag_bytes(1, 0, 0, 7, 0)),
            Parsed::Reject(Why::Fields)
        );
        assert_eq!(
            parse(&tag_bytes(1, 0, 1000, 0, 0)),
            Parsed::Reject(Why::Fields)
        );
    }

    #[test]
    fn parse_is_total_over_arbitrary_bytes() {
        // No panic on any length 0..=40 of pseudo-random bytes.
        let mut x = 0x1234_5678u32;
        for len in 0..=40usize {
            for _ in 0..50 {
                let mut v = [0u8; 40];
                for b in v.iter_mut() {
                    x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    *b = (x >> 24) as u8;
                }
                let _ = parse(&v[..len]);
            }
        }
    }

    // ---- verify ------------------------------------------------------------------------

    #[test]
    fn the_exact_claim_is_honoured() {
        assert_eq!(verify(&good(), &facts()), Verdict::Skip);
    }

    #[test]
    fn a_tag_a_few_frames_behind_the_newest_is_honoured_up_to_the_lag() {
        let mut f = facts();
        f.live.as_mut().unwrap().newest_sequence = 1000 + MAX_LAG;
        assert_eq!(verify(&good(), &f), Verdict::Skip);
        f.live.as_mut().unwrap().newest_sequence = 1000 + MAX_LAG + 1;
        assert_eq!(verify(&good(), &f), Verdict::Reject(Why::Stale));
    }

    #[test]
    fn a_sequence_the_source_never_minted_is_ahead() {
        let mut t = good();
        t.sequence = 1001;
        assert_eq!(verify(&t, &facts()), Verdict::Reject(Why::Ahead));
        t.sequence = u64::MAX;
        assert_eq!(verify(&t, &facts()), Verdict::Reject(Why::Ahead));
    }

    #[test]
    fn the_wrong_generation_is_rejected_whatever_the_sequence() {
        let mut t = good();
        t.generation = 6;
        assert_eq!(verify(&t, &facts()), Verdict::Reject(Why::Generation));
        t.generation = 8;
        assert_eq!(verify(&t, &facts()), Verdict::Reject(Why::Generation));
    }

    #[test]
    fn another_process_is_not_the_owner() {
        let mut f = facts();
        f.presenter_process = 0xBB;
        assert_eq!(verify(&good(), &f), Verdict::Reject(Why::Owner));
    }

    #[test]
    fn an_unknown_process_never_matches_even_another_unknown_one() {
        let mut f = facts();
        f.presenter_process = 0;
        f.live.as_mut().unwrap().process = 0;
        assert_eq!(verify(&good(), &f), Verdict::Reject(Why::Owner));
        f.presenter_process = 0xAA;
        assert_eq!(verify(&good(), &f), Verdict::Reject(Why::Owner));
        f.presenter_process = 0;
        f.live.as_mut().unwrap().process = 0xAA;
        assert_eq!(verify(&good(), &f), Verdict::Reject(Why::Owner));
    }

    #[test]
    fn no_live_source_means_no_skip() {
        let mut f = facts();
        f.live = None;
        assert_eq!(verify(&good(), &f), Verdict::Reject(Why::NoSource));
        // A live source that has minted nothing yet cannot back any sequence.
        let mut f = facts();
        f.live.as_mut().unwrap().newest_sequence = 0;
        assert_eq!(verify(&good(), &f), Verdict::Reject(Why::NoSource));
    }

    #[test]
    fn a_flip_present_is_never_skipped() {
        for arm in [Arm::FlipMmio, Arm::FlipDma] {
            let mut f = facts();
            f.arm = arm;
            assert_eq!(verify(&good(), &f), Verdict::Reject(Why::NotBlt), "{arm:?}");
        }
    }

    #[test]
    fn color_fill_sub_rects_and_snapshot_blts_are_not_skipped() {
        let mut f = facts();
        f.color_fill = true;
        assert_eq!(verify(&good(), &f), Verdict::Reject(Why::ColorFill));
        let mut f = facts();
        f.sub_rects = true;
        assert_eq!(verify(&good(), &f), Verdict::Reject(Why::SubRects));
        let mut f = facts();
        f.snapshot = true;
        assert_eq!(verify(&good(), &f), Verdict::Reject(Why::Snapshot));
    }

    #[test]
    fn a_named_resource_must_be_the_source() {
        let mut t = good();
        t.resource_id = 55;
        assert_eq!(verify(&t, &facts()), Verdict::Skip);
        t.resource_id = 56;
        assert_eq!(verify(&t, &facts()), Verdict::Reject(Why::Resource));
        // The source did not resolve: a named resource cannot match it.
        let mut f = facts();
        f.source_resource_id = 0;
        assert_eq!(verify(&t, &f), Verdict::Reject(Why::Resource));
        // An unnamed one does not care.
        t.resource_id = 0;
        assert_eq!(verify(&t, &f), Verdict::Skip);
    }

    #[test]
    fn the_first_failing_check_is_the_reason() {
        // Everything wrong at once: the arm is named first, then the shape, then the source facts.
        let mut t = good();
        t.generation = 9;
        t.sequence = 5000;
        t.resource_id = 1;
        let mut f = facts();
        f.arm = Arm::FlipDma;
        f.color_fill = true;
        f.presenter_process = 0xBB;
        assert_eq!(verify(&t, &f), Verdict::Reject(Why::NotBlt));
        f.arm = Arm::Blt;
        assert_eq!(verify(&t, &f), Verdict::Reject(Why::ColorFill));
        f.color_fill = false;
        assert_eq!(verify(&t, &f), Verdict::Reject(Why::Generation));
        t.generation = 7;
        assert_eq!(verify(&t, &f), Verdict::Reject(Why::Owner));
        f.presenter_process = 0xAA;
        assert_eq!(verify(&t, &f), Verdict::Reject(Why::Ahead));
        t.sequence = 1000;
        assert_eq!(verify(&t, &f), Verdict::Reject(Why::Resource));
    }

    #[test]
    fn a_verdict_never_reads_the_tag_for_anything_but_its_three_fields() {
        // Skip is a function of (tag, facts) only: the same inputs, the same answer, repeatedly.
        for _ in 0..3 {
            assert_eq!(verify(&good(), &facts()), Verdict::Skip);
        }
    }

    // ---- the record --------------------------------------------------------------------

    #[test]
    fn shown_follows_generations_and_never_goes_backwards() {
        let mut s = Shown::new();
        assert_eq!(s.live_for(1), None);
        s.minted(1, 0xAA, 10);
        s.minted(1, 0xAA, 12);
        s.minted(1, 0xAA, 11);
        assert_eq!(
            s.live_for(1),
            Some(Live {
                generation: 1,
                process: 0xAA,
                newest_sequence: 12
            })
        );
        // Another generation is another source: the record restarts, the old one is gone.
        s.minted(2, 0xBB, 40);
        assert_eq!(s.live_for(1), None);
        assert_eq!(s.live_for(2).unwrap().newest_sequence, 40);
        assert_eq!(s.live_for(2).unwrap().process, 0xBB);
        // Generation 0 is nothing.
        s.minted(0, 0xCC, 99);
        assert_eq!(s.live_for(0), None);
        assert_eq!(s.live_for(2).unwrap().newest_sequence, 40);
    }

    #[test]
    fn a_live_source_with_no_minted_frame_in_the_record_is_not_live_for_a_tag() {
        // The state machine says generation 3 is live, the record knows only generation 2.
        let mut s = Shown::new();
        s.minted(2, 0xAA, 10);
        let f = Facts {
            live: s.live_for(3),
            ..facts()
        };
        let t = Tag {
            generation: 3,
            ..good()
        };
        assert_eq!(verify(&t, &f), Verdict::Reject(Why::NoSource));
    }

    #[test]
    fn extent_bytes_follows_the_pitch_when_it_covers_the_row() {
        assert_eq!(extent_bytes(5120, 1440, 20480), 20480 * 1440);
        assert_eq!(extent_bytes(5120, 1440, 21504), 21504 * 1440);
        // A pitch below the row (or zero) falls back to 4 bytes a pixel.
        assert_eq!(extent_bytes(5120, 1440, 0), 5120 * 4 * 1440);
        assert_eq!(extent_bytes(5120, 1440, 100), 5120 * 4 * 1440);
        assert_eq!(extent_bytes(0, 0, 0), 0);
        // 4 * MAX * MAX does not fit u64: saturating, not wrapping.
        assert_eq!(extent_bytes(u32::MAX, u32::MAX, u32::MAX), u64::MAX);
    }

    // ---- reason codes and counters -------------------------------------------------------

    #[test]
    fn reason_codes_are_stable_and_dense() {
        let all = [
            Why::BadMagic,
            Why::Short,
            Why::Version,
            Why::Flags,
            Why::Fields,
            Why::NotBlt,
            Why::ColorFill,
            Why::SubRects,
            Why::Snapshot,
            Why::NoSource,
            Why::Generation,
            Why::Owner,
            Why::Ahead,
            Why::Stale,
            Why::Resource,
            Why::Orphan,
            Why::Retry,
        ];
        assert_eq!(all.len(), WHY_COUNT);
        for (i, w) in all.iter().enumerate() {
            assert_eq!(w.code() as usize, i + 1);
        }
    }

    #[test]
    fn counter_names_fit_are_unique_and_collide_with_nothing_else() {
        let names: Vec<std::string::String> = COUNTERS.iter().map(|s| (*s).into()).collect();
        for n in &names {
            assert!(n.len() <= 14, "{n} is longer than 14");
            assert!(n.starts_with("Os"));
        }
        let mut sorted = names.clone();
        sorted.sort();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "duplicate counter name");
        // Nothing else in either crate writes a name starting with `Os` (a literal in another
        // file would merge with ours): scan both trees, allowing only this module and its I/O half.
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut scanned = 0usize;
        for root in [manifest.join("src"), manifest.join("../kmd_render/src")] {
            if !root.exists() {
                continue; // a copy of this crate without its sibling: nothing to scan
            }
            let mut stack = Vec::new();
            stack.push(root);
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                        continue;
                    }
                    let owner = path.ends_with("onscanout.rs");
                    let text = std::fs::read_to_string(&path).unwrap();
                    scanned += 1;
                    for lit in text.split("b\"").skip(1) {
                        let name: std::string::String = lit
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric())
                            .collect();
                        if name.starts_with("Os") && !owner {
                            panic!(
                                "{} writes {name}: the `Os` prefix is onscanout's",
                                path.display()
                            );
                        }
                    }
                }
            }
        }
        assert!(scanned > 0);
    }
}
