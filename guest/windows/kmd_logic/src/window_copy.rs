//! The GPU-ordered window copy (`NvkBltGpuCopy`, tag `HOWW`): the pure half. The record of where
//! the KMD's windowed Blt of an NVK back buffer lands, the parse of the tag, and the verdict on a
//! tagged Present. No memory, no transport, no clock: the I/O half is
//! `kmd_render/src/ddi/window_copy.rs`, the wire `protocol/src/window_copy.rs`.
//!
//! THE RECORD. Every redirected Blt of an NVK image into a VRAM redirection surface (the
//! `RedirVram` foreign arm) notes `(source, destination, process, rectangles, surface facts)`
//! here; an entry whose content changes gets a new ticket. The producer asks for its source's
//! entry (`HELIOS_ESCAPE_BLT_TARGET`), imports the destination by resource id ([`Table::granted`]
//! lets it: it presented into that surface), copies the frame into it on its own queue and tags
//! the Present with the ticket.
//!
//! THE VERDICT. A tagged Present is completed with no copy only when the KMD's own state backs
//! the whole claim ([`verify`]): a Blt (not a flip) from an NVK image into a VRAM surface, the
//! Present's source and destination are the tag's, the record for that source and the presenting
//! process names that destination with the tag's ticket and the Present's rectangles, and the
//! Present carries a boundary (its RM fence was attached), so its DMA buffer still retires only
//! after the producer's copy. Anything else is the ordinary Blt. What a lying tag can cost is
//! the update of the liar's own window surface for one frame: the destination is dxgkrnl's, from
//! the Present's allocation list, never the tag's.

use crate::rm_vidmem::Rect;

/// `'HOWW'` (`protocol::HELIOS_WINDOW_WRITTEN_MAGIC`; pinned by the KMD build).
pub const MAGIC: u32 = 0x5757_4F48;
/// `protocol::HELIOS_WINDOW_WRITTEN_VERSION`.
pub const VERSION: u16 = 1;
/// The tag's size.
pub const TAG_BYTES: usize = 24;
/// Where the tag sits in a `HERF` command (the on-scanout tag's slot).
pub const HERF_OFFSET: usize = 48;
/// `CommandLength` of a `HERF` command that carries the whole tag.
pub const HERF_BYTES: usize = HERF_OFFSET + TAG_BYTES;
/// Entries of the record. One per back buffer of a windowed NVK swap chain (a blt-model chain
/// rotates its buffers' identities, so each buffer is a source), least recently used replaced.
pub const TABLE_CAP: usize = 16;

/// A parsed, well-formed tag.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tag {
    pub source: u32,
    pub destination: u32,
    pub ticket: u32,
}

/// Why a tag was not honoured: the value of `GwRejWhy`. Stable, never renumbered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Why {
    /// `HOWW` but fewer than [`TAG_BYTES`] bytes of command.
    Short = 1,
    /// A version this KMD does not know.
    Version = 2,
    /// A flags or reserved bit set.
    Flags = 3,
    /// A zero source, destination or ticket.
    Fields = 4,
    /// The Present is a flip, a `ColorFill`, or a Blt with no allocation list.
    NotBlt = 5,
    /// The Present's source or destination did not resolve, the source is not an NVK image or
    /// the destination is not a VRAM redirection surface.
    Surfaces = 6,
    /// The Present's source is not the tag's.
    Source = 7,
    /// The Present's destination is not the tag's.
    Destination = 8,
    /// No record for this source and process, or it names another destination.
    NoRecord = 9,
    /// The record's ticket is not the tag's (the record changed after the producer asked).
    Ticket = 10,
    /// The Present's rectangles are not the record's.
    Rect = 11,
    /// The Present carries no boundary (no attached RM fence): nothing would order a reader
    /// of the surface after the producer's copy.
    NoBoundary = 12,
    /// The Render's stash was replaced or dropped before a Present took it.
    Orphan = 13,
    /// The Present could not take the skip (DMA or private buffer too small, patch capacity):
    /// dxgkrnl retries it without the tag, and the retry is the ordinary Blt.
    Retry = 14,
}

/// How many reasons there are (codes 1 ..= `WHY_COUNT`).
pub const WHY_COUNT: usize = 14;

impl Why {
    pub const fn code(self) -> u32 {
        self as u32
    }
}

/// What the tail of a `HERF` command held at [`HERF_OFFSET`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Parsed {
    /// No `HOWW` claim: the command ends before the slot, the slot is zero, or it holds another
    /// tag (`HOSC`, whose own parser answers for it).
    Absent,
    /// A well-formed claim, not yet verified.
    Tag(Tag),
    /// A malformed `HOWW` claim: counted, never stashed.
    Reject(Why),
}

fn u16_at(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

/// Whether the slot's first four bytes are the `HOWW` magic (the on-scanout parser treats such a
/// slot as absent).
pub fn is_tag_magic(tail: &[u8]) -> bool {
    tail.len() >= 4 && u32_at(tail, 0) == MAGIC
}

/// Parse the bytes of a `HERF` command from [`HERF_OFFSET`] to its `CommandLength` (any length;
/// only the first [`TAG_BYTES`] are looked at). Little-endian.
pub fn parse(tail: &[u8]) -> Parsed {
    if !is_tag_magic(tail) {
        return Parsed::Absent;
    }
    if tail.len() < TAG_BYTES {
        return Parsed::Reject(Why::Short);
    }
    if u16_at(tail, 4) != VERSION {
        return Parsed::Reject(Why::Version);
    }
    if u16_at(tail, 6) != 0 || u32_at(tail, 20) != 0 {
        return Parsed::Reject(Why::Flags);
    }
    let tag = Tag {
        source: u32_at(tail, 8),
        destination: u32_at(tail, 12),
        ticket: u32_at(tail, 16),
    };
    if tag.source == 0 || tag.destination == 0 || tag.ticket == 0 {
        return Parsed::Reject(Why::Fields);
    }
    Parsed::Tag(tag)
}

/// What an import of the destination needs: the same facts its open identity hands DWM.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Surface {
    pub width: u32,
    pub height: u32,
    pub pitch: u32,
    pub dxgi_format: u32,
    pub bind_flags: u32,
    pub memory_type_index: u32,
    pub alloc_size: u64,
    pub modifier: u64,
    pub offset: u32,
    pub fourcc: u32,
}

/// One recorded Blt: `source` (in the presenting `process`) lands in `destination`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub source: u32,
    pub destination: u32,
    /// `hKmdProcess` of the presenting context. Nonzero.
    pub process: u64,
    pub src: Rect,
    pub dst: Rect,
    pub surface: Surface,
    /// Names this content; a changed entry gets a new one. Nonzero.
    pub ticket: u32,
}

impl Target {
    fn same_content(&self, other: &Target) -> bool {
        self.destination == other.destination
            && self.src == other.src
            && self.dst == other.dst
            && self.surface == other.surface
    }
}

/// The record: at most [`TABLE_CAP`] entries keyed by `(source, process)`.
#[derive(Clone, Copy, Debug)]
pub struct Table {
    entries: [Option<(Target, u32)>; TABLE_CAP],
    next_ticket: u32,
    clock: u32,
}

impl Default for Table {
    fn default() -> Self {
        Self::new()
    }
}

impl Table {
    pub const fn new() -> Self {
        Self {
            entries: [None; TABLE_CAP],
            next_ticket: 0,
            clock: 0,
        }
    }

    fn mint(&mut self) -> u32 {
        self.next_ticket = self.next_ticket.wrapping_add(1);
        if self.next_ticket == 0 {
            self.next_ticket = 1;
        }
        self.next_ticket
    }

    fn tick(&mut self) -> u32 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    /// A Blt from `t.source` (of `t.process`) into `t.destination` happened (`t.ticket` is
    /// ignored). Returns the entry's ticket: the old one when nothing changed, else a new one.
    /// A zero source, destination or process records nothing (returns 0).
    pub fn note_blt(&mut self, mut t: Target) -> u32 {
        if t.source == 0 || t.destination == 0 || t.process == 0 {
            return 0;
        }
        let now = self.tick();
        let found = self
            .entries
            .iter()
            .position(|s| s.is_some_and(|(e, _)| e.source == t.source && e.process == t.process));
        if let Some(i) = found {
            if let Some((old, _)) = self.entries[i] {
                if old.same_content(&t) {
                    self.entries[i] = Some((old, now));
                    return old.ticket;
                }
            }
            t.ticket = self.mint();
            self.entries[i] = Some((t, now));
            return t.ticket;
        }
        t.ticket = self.mint();
        let index = match self.entries.iter().position(Option::is_none) {
            Some(i) => i,
            // The least recently used: the largest age.
            None => self
                .entries
                .iter()
                .enumerate()
                .max_by_key(|(_, e)| e.map_or(u32::MAX, |(_, used)| now.wrapping_sub(used)))
                .map_or(0, |(i, _)| i),
        };
        self.entries[index] = Some((t, now));
        t.ticket
    }

    /// The entry for `source` of `process`.
    pub fn lookup(&self, source: u32, process: u64) -> Option<Target> {
        if source == 0 || process == 0 {
            return None;
        }
        self.entries
            .iter()
            .flatten()
            .find(|(e, _)| e.source == source && e.process == process)
            .map(|(e, _)| *e)
    }

    /// Whether `process` presented into `destination` (and so may import it by resource id).
    pub fn granted(&self, process: u64, destination: u32) -> bool {
        process != 0
            && destination != 0
            && self
                .entries
                .iter()
                .flatten()
                .any(|(e, _)| e.process == process && e.destination == destination)
    }

    /// The surface `destination` is gone: forget every entry naming it.
    pub fn destination_gone(&mut self, destination: u32) -> usize {
        let mut n = 0;
        for slot in self.entries.iter_mut() {
            if slot.is_some_and(|(e, _)| e.destination == destination) {
                *slot = None;
                n += 1;
            }
        }
        n
    }

    pub fn len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// What the KMD knows about the tagged Present, from its own state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    /// A Blt with an allocation list, not a flip and not a `ColorFill`.
    pub blt: bool,
    /// Source and destination resolved, the source is an NVK image, the destination a VRAM
    /// redirection surface.
    pub surfaces: bool,
    pub source: u32,
    pub destination: u32,
    pub process: u64,
    /// The Present's rectangles (`None`: outside the surfaces, or a stretch).
    pub rects: Option<(Rect, Rect)>,
    /// The Present carries a boundary (the RM fence tail attached a gate point).
    pub boundary: bool,
}

/// The verdict on a tagged Present.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Complete the Blt with no copy: the producer wrote the destination before the fence.
    NoCopy,
    /// The ordinary Blt.
    Reject(Why),
}

/// May the Present the tag rides on be completed with no copy? `record` is
/// `Table::lookup(facts.source, facts.process)`.
pub fn verify(tag: &Tag, record: Option<&Target>, facts: &Facts) -> Verdict {
    if !facts.blt {
        return Verdict::Reject(Why::NotBlt);
    }
    if !facts.surfaces {
        return Verdict::Reject(Why::Surfaces);
    }
    if facts.source != tag.source {
        return Verdict::Reject(Why::Source);
    }
    if facts.destination != tag.destination {
        return Verdict::Reject(Why::Destination);
    }
    let Some(r) = record.filter(|r| {
        r.source == facts.source && r.process == facts.process && r.destination == facts.destination
    }) else {
        return Verdict::Reject(Why::NoRecord);
    };
    if r.ticket != tag.ticket {
        return Verdict::Reject(Why::Ticket);
    }
    if facts.rects != Some((r.src, r.dst)) {
        return Verdict::Reject(Why::Rect);
    }
    if !facts.boundary {
        return Verdict::Reject(Why::NoBoundary);
    }
    Verdict::NoCopy
}

/// The service-key counter names the I/O half writes (`kmd_render/src/ddi/window_copy.rs`): at
/// most 14 characters, prefix `Gw`, unique across `kmd_render` and `kmd_logic`.
/// `GwTag` claims seen, `GwSkip` Presents completed with no copy, `GwRej` claims refused,
/// `GwRejWhy` the last reason, `GwWhyMask` every reason seen (bit `code - 1`), `GwQuery` target
/// queries, `GwQFound` queries answered with a record, `GwGrant` imports allowed by the record,
/// `GwNote` Blts recorded, `GwTicket` the newest ticket.
pub const COUNTERS: [&str; 10] = [
    "GwTag",
    "GwSkip",
    "GwRej",
    "GwRejWhy",
    "GwWhyMask",
    "GwQuery",
    "GwQFound",
    "GwGrant",
    "GwNote",
    "GwTicket",
];

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    const P: u64 = 0xA0;
    const Q: u64 = 0xB0;

    fn rect(l: u32, t: u32, w: u32, h: u32) -> Rect {
        Rect {
            left: l,
            top: t,
            right: l + w,
            bottom: t + h,
        }
    }

    fn target(source: u32, destination: u32, process: u64) -> Target {
        Target {
            source,
            destination,
            process,
            src: rect(0, 0, 1600, 900),
            dst: rect(8, 31, 1600, 900),
            surface: Surface {
                width: 1616,
                height: 939,
                pitch: 6464,
                dxgi_format: 87,
                ..Surface::default()
            },
            ticket: 0,
        }
    }

    fn tag_bytes(magic: u32, version: u16, flags: u16, s: u32, d: u32, t: u32, r: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&magic.to_le_bytes());
        b.extend_from_slice(&version.to_le_bytes());
        b.extend_from_slice(&flags.to_le_bytes());
        b.extend_from_slice(&s.to_le_bytes());
        b.extend_from_slice(&d.to_le_bytes());
        b.extend_from_slice(&t.to_le_bytes());
        b.extend_from_slice(&r.to_le_bytes());
        b
    }

    #[test]
    fn magic_is_howw() {
        assert_eq!(&MAGIC.to_le_bytes(), b"HOWW");
        assert_ne!(MAGIC, crate::onscanout::MAGIC);
        assert_eq!(HERF_OFFSET, crate::onscanout::HERF_OFFSET);
        assert_eq!(HERF_BYTES, crate::onscanout::HERF_BYTES);
    }

    #[test]
    fn parse_takes_only_well_formed_howw() {
        assert_eq!(parse(&[]), Parsed::Absent);
        assert_eq!(parse(&[0; 24]), Parsed::Absent);
        // Another tag in the slot is not ours.
        let hosc = tag_bytes(crate::onscanout::MAGIC, 1, 0, 1, 2, 3, 0);
        assert_eq!(parse(&hosc), Parsed::Absent);
        let good = tag_bytes(MAGIC, 1, 0, 5, 6, 7, 0);
        assert_eq!(
            parse(&good),
            Parsed::Tag(Tag {
                source: 5,
                destination: 6,
                ticket: 7
            })
        );
        assert_eq!(parse(&good[..20]), Parsed::Reject(Why::Short));
        assert_eq!(
            parse(&tag_bytes(MAGIC, 2, 0, 5, 6, 7, 0)),
            Parsed::Reject(Why::Version)
        );
        assert_eq!(
            parse(&tag_bytes(MAGIC, 1, 1, 5, 6, 7, 0)),
            Parsed::Reject(Why::Flags)
        );
        assert_eq!(
            parse(&tag_bytes(MAGIC, 1, 0, 5, 6, 7, 9)),
            Parsed::Reject(Why::Flags)
        );
        assert_eq!(
            parse(&tag_bytes(MAGIC, 1, 0, 0, 6, 7, 0)),
            Parsed::Reject(Why::Fields)
        );
        assert_eq!(
            parse(&tag_bytes(MAGIC, 1, 0, 5, 6, 0, 0)),
            Parsed::Reject(Why::Fields)
        );
    }

    #[test]
    fn the_on_scanout_parser_leaves_howw_alone() {
        let good = tag_bytes(MAGIC, 1, 0, 5, 6, 7, 0);
        assert_eq!(
            crate::onscanout::parse(&good),
            crate::onscanout::Parsed::Absent
        );
    }

    #[test]
    fn a_ticket_survives_an_unchanged_blt_and_changes_with_the_content() {
        let mut t = Table::new();
        let a = t.note_blt(target(1, 10, P));
        assert_ne!(a, 0);
        assert_eq!(t.note_blt(target(1, 10, P)), a);
        // Another source of the same chain: its own ticket.
        let b = t.note_blt(target(2, 10, P));
        assert_ne!(b, a);
        // The window got a new surface: new ticket.
        let c = t.note_blt(target(1, 11, P));
        assert_ne!(c, a);
        assert_eq!(t.lookup(1, P).unwrap().destination, 11);
        assert_eq!(t.lookup(1, P).unwrap().ticket, c);
        // Moved client area: new ticket.
        let mut moved = target(1, 11, P);
        moved.dst = rect(0, 0, 1600, 900);
        let d = t.note_blt(moved);
        assert_ne!(d, c);
        assert_eq!(t.len(), 2);
        // Nothing recorded for zero ids.
        assert_eq!(t.note_blt(target(0, 11, P)), 0);
        assert_eq!(t.note_blt(target(1, 11, 0)), 0);
    }

    #[test]
    fn lookups_and_grants_are_per_process() {
        let mut t = Table::new();
        t.note_blt(target(1, 10, P));
        assert!(t.lookup(1, P).is_some());
        assert!(t.lookup(1, Q).is_none());
        assert!(t.granted(P, 10));
        assert!(!t.granted(Q, 10));
        assert!(!t.granted(P, 11));
        assert!(!t.granted(0, 10));
        assert_eq!(t.destination_gone(10), 1);
        assert!(!t.granted(P, 10));
        assert!(t.is_empty());
    }

    #[test]
    fn a_full_table_replaces_the_least_recently_used() {
        let mut t = Table::new();
        for s in 1..=TABLE_CAP as u32 {
            t.note_blt(target(s, 100 + s, P));
        }
        // Touch source 1 so source 2 is the oldest.
        t.note_blt(target(1, 101, P));
        t.note_blt(target(99, 199, P));
        assert_eq!(t.len(), TABLE_CAP);
        assert!(t.lookup(1, P).is_some());
        assert!(t.lookup(2, P).is_none());
        assert!(t.lookup(99, P).is_some());
    }

    fn facts_for(r: &Target) -> Facts {
        Facts {
            blt: true,
            surfaces: true,
            source: r.source,
            destination: r.destination,
            process: r.process,
            rects: Some((r.src, r.dst)),
            boundary: true,
        }
    }

    #[test]
    fn verify_honours_only_a_claim_the_record_backs() {
        let mut t = Table::new();
        let ticket = t.note_blt(target(1, 10, P));
        let r = t.lookup(1, P).unwrap();
        let tag = Tag {
            source: 1,
            destination: 10,
            ticket,
        };
        let f = facts_for(&r);
        assert_eq!(verify(&tag, Some(&r), &f), Verdict::NoCopy);
        let rej = |f: Facts, rec: Option<&Target>, tag: Tag| verify(&tag, rec, &f);
        assert_eq!(
            rej(Facts { blt: false, ..f }, Some(&r), tag),
            Verdict::Reject(Why::NotBlt)
        );
        assert_eq!(
            rej(
                Facts {
                    surfaces: false,
                    ..f
                },
                Some(&r),
                tag
            ),
            Verdict::Reject(Why::Surfaces)
        );
        assert_eq!(
            rej(Facts { source: 2, ..f }, Some(&r), tag),
            Verdict::Reject(Why::Source)
        );
        assert_eq!(
            rej(
                Facts {
                    destination: 11,
                    ..f
                },
                Some(&r),
                tag
            ),
            Verdict::Reject(Why::Destination)
        );
        assert_eq!(rej(f, None, tag), Verdict::Reject(Why::NoRecord));
        // Another process's record does not back this one's Present.
        assert_eq!(
            rej(Facts { process: Q, ..f }, Some(&r), tag),
            Verdict::Reject(Why::NoRecord)
        );
        assert_eq!(
            rej(
                f,
                Some(&r),
                Tag {
                    ticket: ticket + 1,
                    ..tag
                }
            ),
            Verdict::Reject(Why::Ticket)
        );
        assert_eq!(
            rej(Facts { rects: None, ..f }, Some(&r), tag),
            Verdict::Reject(Why::Rect)
        );
        assert_eq!(
            rej(
                Facts {
                    rects: Some((r.src, rect(0, 0, 1600, 900))),
                    ..f
                },
                Some(&r),
                tag
            ),
            Verdict::Reject(Why::Rect)
        );
        assert_eq!(
            rej(
                Facts {
                    boundary: false,
                    ..f
                },
                Some(&r),
                tag
            ),
            Verdict::Reject(Why::NoBoundary)
        );
    }

    #[test]
    fn reason_codes_are_stable_and_dense() {
        let all = [
            Why::Short,
            Why::Version,
            Why::Flags,
            Why::Fields,
            Why::NotBlt,
            Why::Surfaces,
            Why::Source,
            Why::Destination,
            Why::NoRecord,
            Why::Ticket,
            Why::Rect,
            Why::NoBoundary,
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
            assert!(n.starts_with("Gw"));
        }
        let mut sorted = names.clone();
        sorted.sort();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(sorted.len(), before, "duplicate counter name");
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut scanned = 0usize;
        for root in [manifest.join("src"), manifest.join("../kmd_render/src")] {
            if !root.exists() {
                continue;
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
                    let owner = path.ends_with("window_copy.rs");
                    let text = std::fs::read_to_string(&path).unwrap();
                    scanned += 1;
                    for lit in text.split("b\"").skip(1) {
                        let name: std::string::String = lit
                            .chars()
                            .take_while(|c| c.is_ascii_alphanumeric())
                            .collect();
                        if name.starts_with("Gw") {
                            assert!(
                                owner,
                                "{} writes {name}: `Gw` is window_copy's",
                                path.display()
                            );
                            assert!(names.contains(&name), "{name} is not in COUNTERS");
                        }
                    }
                }
            }
        }
        assert!(scanned > 0);
    }
}
