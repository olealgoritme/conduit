//! `NvPgCheck`: content checks of native paging operations and of submitted GPFIFO entries.
//!
//! The I/O half is `kmd_render` `ddi/pg_check.rs`. Knob bits:
//!
//! * `0x1` paging: after every native TRANSFER / FILL / VIRTUAL_FILL / VIRTUAL_TRANSFER that
//!   ran (CPU or copy-engine path), sampled pieces of both ends are read back and compared
//!   (a transfer: the system pages against the allocation's RM memory, by checksum; a fill:
//!   the RM memory against the pattern). `PgChkN` checks, `PgChkBad` mismatches, `PgChkLast`
//!   the last mismatch's [`op_word`], `PgChkId` / `PgChkOff` its allocation and byte offset.
//! * `0x2` (`0x4` too) submit: after every native `SubmitCommandToHwQueue` the new GPFIFO
//!   entries `[old GP_PUT, new GP_PUT)` are read through the GPU-side mapping (VidMm's system
//!   pages, or the RM memory) and classified ([`gp_verdict`]). `HwsGpChk` submissions checked,
//!   `HwsZeroGp` submissions with a zero entry, `HwsZeroGpE` zero entries, `HwsZeroGpLast`
//!   submissions whose LAST entry reads zero, `HwsZeroGpAll` submissions that read all zero,
//!   `HwsZeroGpOp` the ring allocation's last paging [`op_word`] at the last such submission,
//!   `HwsZeroGpVa` its GPFIFO entry VA (low 32 bits).
//!
//! The NVIDIA UMDs leave runs of all-zero GPFIFO entries (length 0, control opcode 0 = NOP)
//! inside their submissions: a zero entry alone is not lost content. A write that never
//! reached the pages the GPU fetches would also take the submission's last entry with it,
//! which is what `HwsZeroGpLast` / `HwsZeroGpAll` count.

/// Knob bits.
pub const PAGING: u32 = 0x1;
pub const SUBMIT: u32 = 0x2;
pub const SUBMIT_ALT: u32 = 0x4;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Knob(pub u32);

impl Knob {
    pub const fn paging(self) -> bool {
        self.0 & PAGING != 0
    }

    pub const fn submit(self) -> bool {
        self.0 & (SUBMIT | SUBMIT_ALT) != 0
    }

    pub const fn any(self) -> bool {
        self.paging() || self.submit()
    }
}

/// Paging operation kinds (the `NvPgFail` numbering).
pub mod op {
    pub const LEAF: u32 = 1;
    pub const TRANSFER: u32 = 2;
    pub const FILL: u32 = 3;
    pub const VIRTUAL_FILL: u32 = 4;
    pub const VIRTUAL_TRANSFER: u32 = 5;
}

/// Where the allocation's last leaf update placed it.
pub mod place {
    pub const UNKNOWN: u32 = 0;
    /// VidMm's system pages (an OS-descriptor mapping, `update_leaf_system`).
    pub const SYSTEM: u32 = 1;
    /// The allocation's RM memory in its own segment.
    pub const SEGMENT: u32 = 2;
    /// The aperture segment.
    pub const APERTURE: u32 = 3;
    /// Invalid PTEs only (unmapped).
    pub const INVALID: u32 = 4;
    /// System pages and something else in one update.
    pub const MIXED: u32 = 5;
}

/// Direction of a content operation.
pub mod dir {
    pub const NONE: u32 = 0;
    /// System pages to the RM memory (page-in).
    pub const TO_NATIVE: u32 = 1;
    /// The RM memory to system pages (eviction).
    pub const FROM_NATIVE: u32 = 2;
}

/// Which engine ran it.
pub mod path {
    pub const NONE: u32 = 0;
    pub const CPU: u32 = 1;
    pub const GPU: u32 = 2;
    /// Refused (answered success without running).
    pub const REFUSED: u32 = 3;
}

/// One word for an allocation's last paging operation: bits 0..3 the operation ([`op`]),
/// 4..7 the placement of the last leaf update ([`place`]), 8..9 the direction ([`dir`]),
/// 10..11 the path ([`path`]), 16..31 the low 16 bits of the paging sequence number.
pub const fn op_word(op: u32, place: u32, dir: u32, path: u32, seq: u32) -> u32 {
    (op & 0xf) | (place & 0xf) << 4 | (dir & 0x3) << 8 | (path & 0x3) << 10 | (seq & 0xffff) << 16
}

pub const fn word_op(word: u32) -> u32 {
    word & 0xf
}

pub const fn word_place(word: u32) -> u32 {
    (word >> 4) & 0xf
}

pub const fn word_dir(word: u32) -> u32 {
    (word >> 8) & 0x3
}

pub const fn word_path(word: u32) -> u32 {
    (word >> 10) & 0x3
}

/// `word` with its placement replaced (a leaf update keeps the last content operation).
pub const fn with_place(word: u32, place: u32) -> u32 {
    (word & !0xf0) | (place & 0xf) << 4
}

/// The placement a leaf update's PTEs describe: counts of system-page, aperture, own-segment
/// and invalid entries.
pub const fn placement_of(system: u32, aperture: u32, segment: u32, invalid: u32) -> u32 {
    let kinds = (system != 0) as u32 + (aperture != 0) as u32 + (segment != 0) as u32;
    if kinds > 1 {
        place::MIXED
    } else if system != 0 {
        place::SYSTEM
    } else if aperture != 0 {
        place::APERTURE
    } else if segment != 0 {
        place::SEGMENT
    } else if invalid != 0 {
        place::INVALID
    } else {
        place::UNKNOWN
    }
}

// ── paging checks ────────────────────────────────────────────────────────────────────────────

pub const PAGE: u64 = 4096;
/// Bytes compared per sampled piece.
pub const PIECE: u64 = 256;
/// Pieces per operation at most: the start, the middle and the end.
pub const MAX_PIECES: usize = 3;

/// A sampled piece of a content operation: `rel` bytes into it, `len` bytes, never crossing a
/// page of the system end (whose first byte is `start` bytes into its first page).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Piece {
    pub rel: u64,
    pub len: u64,
}

/// Up to [`MAX_PIECES`] pieces of an operation of `size` bytes: at its start, its middle and
/// its end, each at most [`PIECE`] bytes and inside one system page; distinct, in order.
pub fn pieces(size: u64, start: u64) -> ([Piece; MAX_PIECES], usize) {
    let mut out = [Piece { rel: 0, len: 0 }; MAX_PIECES];
    if size == 0 {
        return (out, 0);
    }
    let start = start % PAGE;
    let last = size.saturating_sub(PIECE.min(size));
    let wanted = [0, (size / 2) & !3, last & !3];
    let mut n = 0usize;
    for rel in wanted {
        // The piece ends at the next system page boundary at the latest.
        let to_page = PAGE - (start + rel) % PAGE;
        let len = PIECE.min(size - rel).min(to_page);
        if len == 0 {
            continue;
        }
        if n > 0 {
            let prev: Piece = out[n - 1];
            if rel < prev.rel + prev.len {
                continue;
            }
        }
        out[n] = Piece { rel, len };
        n += 1;
    }
    (out, n)
}

/// FNV-1a over `bytes`.
pub fn checksum(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Whether `bytes`, `rel` bytes into a fill, hold the fill's 32-bit `pattern` (the pattern
/// repeats from the fill's first byte, as `fill_pattern` writes it).
pub fn fill_matches(bytes: &[u8], rel: u64, pattern: u32) -> bool {
    let p = pattern.to_le_bytes();
    bytes
        .iter()
        .enumerate()
        .all(|(i, b)| *b == p[((rel + i as u64) % 4) as usize])
}

// ── submitted GPFIFO entries ─────────────────────────────────────────────────────────────────

/// Entries read per submission at most: the last ones (the last entry is always read).
pub const MAX_GP_CHECK: u32 = 64;

/// The ring runs `[start, start + count)` (at most two, split at the ring's end) of the last
/// `min(count, MAX_GP_CHECK)` entries of a submission `[before, after)` in a ring of
/// `entries`. `None` for an empty or malformed range.
pub fn gp_runs(before: u32, after: u32, entries: u32) -> Option<[(u32, u32); 2]> {
    if entries == 0 || before > entries || after > entries {
        return None;
    }
    let (before, after) = (before % entries, after % entries);
    if before == after {
        return None;
    }
    let span = (after + entries - before) % entries;
    let take = span.min(MAX_GP_CHECK);
    let first = (after + entries - take) % entries;
    if first + take <= entries {
        Some([(first, take), (0, 0)])
    } else {
        let head = entries - first;
        Some([(first, head), (0, take - head)])
    }
}

/// What the GPU sees of a submission's entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpVerdict {
    pub entries: u32,
    /// Entries whose two words read zero.
    pub zero: u32,
    /// The submission's last entry reads zero.
    pub last_zero: bool,
    /// Every entry read zero.
    pub all_zero: bool,
    /// The first zero entry, from the first entry read.
    pub first_zero: Option<u32>,
}

/// Classify the entries `words` (two dwords each, in ring order, the submission's last entry
/// last).
pub fn gp_verdict(words: &[u32]) -> GpVerdict {
    let n = (words.len() / 2) as u32;
    let mut v = GpVerdict {
        entries: n,
        ..GpVerdict::default()
    };
    if n == 0 {
        return v;
    }
    for i in 0..n {
        let i2 = 2 * i as usize;
        if words[i2] == 0 && words[i2 + 1] == 0 {
            v.zero += 1;
            if v.first_zero.is_none() {
                v.first_zero = Some(i);
            }
        }
    }
    let l = 2 * (n as usize - 1);
    v.last_zero = words[l] == 0 && words[l + 1] == 0;
    v.all_zero = v.zero == n;
    v
}

// ── the last operation per allocation ───────────────────────────────────────────────────────

/// A direct-mapped table of the last paging operation per native allocation id: a slot per
/// `id % N`, a newer id evicts an older one.
pub struct LastOps<const N: usize> {
    ids: [u32; N],
    words: [u32; N],
}

impl<const N: usize> Default for LastOps<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> LastOps<N> {
    pub const fn new() -> Self {
        Self {
            ids: [0; N],
            words: [0; N],
        }
    }

    const fn slot(id: u32) -> usize {
        id as usize % N
    }

    /// Record `word` as `id`'s last content operation, keeping the placement it had.
    pub fn note_op(&mut self, id: u32, word: u32) {
        if id == 0 || N == 0 {
            return;
        }
        let s = Self::slot(id);
        let place = if self.ids[s] == id { word_place(self.words[s]) } else { place::UNKNOWN };
        self.ids[s] = id;
        self.words[s] = if word_place(word) == place::UNKNOWN { with_place(word, place) } else { word };
    }

    /// Record `id`'s placement after a leaf update, keeping its last content operation.
    pub fn note_place(&mut self, id: u32, place: u32) {
        if id == 0 || N == 0 {
            return;
        }
        let s = Self::slot(id);
        let word = if self.ids[s] == id { self.words[s] } else { 0 };
        self.ids[s] = id;
        self.words[s] = with_place(word, place);
    }

    pub fn get(&self, id: u32) -> Option<u32> {
        if id == 0 || N == 0 {
            return None;
        }
        let s = Self::slot(id);
        (self.ids[s] == id).then_some(self.words[s])
    }

    pub fn forget(&mut self, id: u32) {
        if N == 0 {
            return;
        }
        let s = Self::slot(id);
        if self.ids[s] == id {
            self.ids[s] = 0;
            self.words[s] = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn knob_bits() {
        assert!(!Knob(0).any());
        assert!(Knob(1).paging() && !Knob(1).submit());
        assert!(Knob(2).submit() && !Knob(2).paging());
        assert!(Knob(4).submit());
        assert!(Knob(7).paging() && Knob(7).submit());
    }

    #[test]
    fn op_word_round_trips() {
        let w = op_word(op::TRANSFER, place::SYSTEM, dir::FROM_NATIVE, path::GPU, 0x1_2345);
        assert_eq!(word_op(w), op::TRANSFER);
        assert_eq!(word_place(w), place::SYSTEM);
        assert_eq!(word_dir(w), dir::FROM_NATIVE);
        assert_eq!(word_path(w), path::GPU);
        assert_eq!(w >> 16, 0x2345);
        let moved = with_place(w, place::SEGMENT);
        assert_eq!(word_place(moved), place::SEGMENT);
        assert_eq!(word_op(moved), op::TRANSFER);
        assert_eq!(moved >> 16, 0x2345);
    }

    #[test]
    fn placement_classes() {
        assert_eq!(placement_of(3, 0, 0, 0), place::SYSTEM);
        assert_eq!(placement_of(0, 0, 4, 1), place::SEGMENT);
        assert_eq!(placement_of(0, 2, 0, 0), place::APERTURE);
        assert_eq!(placement_of(0, 0, 0, 9), place::INVALID);
        assert_eq!(placement_of(1, 0, 1, 0), place::MIXED);
        assert_eq!(placement_of(0, 0, 0, 0), place::UNKNOWN);
    }

    #[test]
    fn pieces_cover_start_middle_end_inside_system_pages() {
        let (p, n) = pieces(64 * 1024, 0);
        assert_eq!(n, 3);
        assert_eq!(p[0], Piece { rel: 0, len: PIECE });
        assert_eq!(p[1], Piece { rel: 32 * 1024, len: PIECE });
        assert_eq!(p[2], Piece { rel: 64 * 1024 - PIECE, len: PIECE });
        for piece in &p[..n] {
            assert_eq!(piece.rel / PAGE, (piece.rel + piece.len - 1) / PAGE);
        }
    }

    #[test]
    fn pieces_stop_at_a_system_page_end() {
        // The system end starts 0xf80 into its page: the first piece is cut at 0x80 bytes.
        let (p, n) = pieces(0x3000, 0xf80);
        assert!(n >= 1);
        assert_eq!(p[0], Piece { rel: 0, len: 0x80 });
        for piece in &p[..n] {
            let a = 0xf80 + piece.rel;
            assert_eq!(a / PAGE, (a + piece.len - 1) / PAGE);
        }
    }

    #[test]
    fn pieces_of_small_operations_do_not_overlap() {
        let (p, n) = pieces(16, 0);
        assert_eq!(n, 1);
        assert_eq!(p[0], Piece { rel: 0, len: 16 });
        let (p, n) = pieces(300, 0);
        for w in p[..n].windows(2) {
            assert!(w[0].rel + w[0].len <= w[1].rel);
        }
        assert_eq!(pieces(0, 0).1, 0);
    }

    #[test]
    fn checksum_differs_on_any_byte() {
        let a = [1u8, 2, 3, 4];
        let b = [1u8, 2, 3, 5];
        assert_ne!(checksum(&a), checksum(&b));
        assert_eq!(checksum(&a), checksum(&[1, 2, 3, 4]));
    }

    #[test]
    fn fill_pattern_phase() {
        let pattern = 0x4433_2211u32;
        assert!(fill_matches(&[0x11, 0x22, 0x33, 0x44, 0x11], 0, pattern));
        assert!(fill_matches(&[0x33, 0x44, 0x11], 2, pattern));
        assert!(!fill_matches(&[0x11, 0x22, 0x33, 0x00], 0, pattern));
        assert!(fill_matches(&[0, 0, 0, 0], 0, 0));
    }

    #[test]
    fn gp_runs_take_the_last_entries() {
        assert_eq!(gp_runs(133, 154, 8192), Some([(133, 21), (0, 0)]));
        // Longer than the cap: only the last 64.
        assert_eq!(gp_runs(1104, 1567, 8192), Some([(1503, 64), (0, 0)]));
        // Wrapping submission: through the ring's end, then from 0.
        assert_eq!(gp_runs(8190, 3, 8192), Some([(8190, 2), (0, 3)]));
        // GP_PUT == entries is the same slot as 0.
        assert_eq!(gp_runs(8180, 8192, 8192), Some([(8180, 12), (0, 0)]));
        assert_eq!(gp_runs(5, 5, 8192), None);
        assert_eq!(gp_runs(0, 1, 0), None);
        assert_eq!(gp_runs(9000, 1, 8192), None);
    }

    #[test]
    fn verdict_separates_nop_gaps_from_lost_writes() {
        // The UMD's own NOP gap, then real entries: zero entries, last entry fine.
        let w = [0, 0, 0, 0, 0x082c_d200, 0x0001_1a00];
        let v = gp_verdict(&w);
        assert_eq!(v.entries, 3);
        assert_eq!(v.zero, 2);
        assert!(!v.last_zero && !v.all_zero);
        assert_eq!(v.first_zero, Some(0));
        // The last entry zero: what a lost write looks like.
        let w = [0x1776_d280, 0x0001_de00, 0, 0];
        let v = gp_verdict(&w);
        assert!(v.last_zero && !v.all_zero);
        assert_eq!(v.first_zero, Some(1));
        let v = gp_verdict(&[0, 0, 0, 0]);
        assert!(v.all_zero && v.last_zero);
        // A control entry with a sync bit is not a zero entry.
        let v = gp_verdict(&[0, 0x8000_0200]);
        assert_eq!(v.zero, 0);
        assert_eq!(gp_verdict(&[]).entries, 0);
    }

    #[test]
    fn last_ops_keep_op_and_placement_apart() {
        let mut t: LastOps<8> = LastOps::new();
        assert_eq!(t.get(3), None);
        t.note_place(3, place::SYSTEM);
        assert_eq!(t.get(3).map(word_place), Some(place::SYSTEM));
        t.note_op(3, op_word(op::TRANSFER, place::UNKNOWN, dir::TO_NATIVE, path::CPU, 7));
        let w = t.get(3).unwrap();
        assert_eq!(word_op(w), op::TRANSFER);
        assert_eq!(word_place(w), place::SYSTEM);
        t.note_place(3, place::SEGMENT);
        let w = t.get(3).unwrap();
        assert_eq!((word_op(w), word_place(w), word_dir(w)), (op::TRANSFER, place::SEGMENT, dir::TO_NATIVE));
        // 11 shares 3's slot: it replaces it.
        t.note_op(11, op_word(op::FILL, place::UNKNOWN, dir::NONE, path::GPU, 1));
        assert_eq!(t.get(3), None);
        assert_eq!(t.get(11).map(word_place), Some(place::UNKNOWN));
        t.forget(11);
        assert_eq!(t.get(11), None);
        t.note_op(0, 1);
        assert_eq!(t.get(0), None);
    }
}
