//! The guest-physical pages behind an address a guest asks RM to register.
//!
//! RM registers memory by a CPU address, read in the caller's address space.
//! A guest's address means nothing in the backend's, so the guest sends the
//! physical pages behind it instead -- pinned on its side so they cannot move
//! under the GPU -- and the backend builds an address of its own that aliases
//! exactly those pages.
//!
//! The runs travel in the deep block, the same place a second-level buffer
//! goes, announced the same way [`crate::segments`] announces itself: a value
//! `deep_ptr_offset` can never genuinely hold. The deep block normally carries
//! what a pointer inside the parameters *addresses*; here it carries where
//! that pointer's memory physically is, which is a different thing about the
//! same field, so it belongs in the same place rather than in a new one.
//!
//! ```text
//!   u32 count, u32 pad
//!   count x { u64 gpa, u64 len }
//! ```
//!
//! A guest that does not know about this sends no deep block at all and its
//! registration is refused, as it was before.
//!
//! A large buffer can scatter into more runs than one message should carry: a
//! guest's 4 KiB pages are rarely contiguous for long, and 1 GiB of them is
//! tens of thousands of runs. Then the table itself is left in guest memory
//! and the deep block, marked [`PAGE_RUNS_INDIRECT`], is a table of the runs
//! *that table* occupies, in the same format. The backend copies the big table
//! out of guest memory before it reads a word of it, so a guest that rewrites
//! it mid-call changes nothing.

/// `deep_ptr_offset` when the deep block is a table of page runs.
///
/// One less than [`crate::segments::SEGMENTED`], and for the same reason: a
/// parameter block is a few hundred bytes at most, so no genuine offset comes
/// anywhere near either value.
pub const PAGE_RUNS: u32 = u32::MAX - 1;

/// `deep_ptr_offset` when the deep block is a table of the page runs that hold
/// the real table, which lies in guest memory. One less again.
pub const PAGE_RUNS_INDIRECT: u32 = u32::MAX - 2;

/// The most runs one registration may describe.
///
/// A fully fragmented buffer costs one run per page, so this covers 4 MiB at
/// worst and more when the pages coalesce at all. The backend holds the same
/// bound; this one keeps a guest from building a message it will only have
/// refused.
pub const MAX_RUNS: usize = 1024;

/// The most runs an indirect table may describe: as many as fit in the
/// `MAX_RUNS` pages a fully scattered direct table can name, 4 MiB. A buffer
/// that coalesces into runs of 16 KiB on average can be 4 GiB; one whose
/// pages coalesce at all better than that, larger.
pub const MAX_RUNS_INDIRECT: usize = (MAX_RUNS * 4096 - 8) / 16;

/// One run: a guest-physical address and a length, both whole pages.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Run {
    pub gpa: u64,
    pub len: u64,
}

/// Bytes a table of `n` runs takes.
pub const fn encoded_len(n: usize) -> usize {
    8 + n * 16
}

/// Write a table of runs into `out`. `None` if there are too many or `out` is
/// too small.
pub fn encode(out: &mut [u8], runs: &[Run]) -> Option<usize> {
    if runs.len() > MAX_RUNS {
        return None;
    }
    let total = encoded_len(runs.len());
    let out = out.get_mut(..total)?;
    out.fill(0);
    out[0..4].copy_from_slice(&(runs.len() as u32).to_le_bytes());
    for (i, r) in runs.iter().enumerate() {
        let e = 8 + i * 16;
        out[e..e + 8].copy_from_slice(&r.gpa.to_le_bytes());
        out[e + 8..e + 16].copy_from_slice(&r.len.to_le_bytes());
    }
    Some(total)
}

/// A table of runs, as it arrived.
#[derive(Clone, Copy, Debug)]
pub struct Runs<'a> {
    bytes: &'a [u8],
    count: usize,
}

impl<'a> Runs<'a> {
    /// Read a table of runs.
    ///
    /// The count is checked against the block before any run is handed out, so
    /// a caller iterating these cannot be pointed past what the guest sent.
    /// `None` means the block is malformed, and a malformed block is a refusal
    /// rather than something to repair: there is no way to guess what was
    /// meant, and guessing here maps the wrong memory.
    pub fn parse(bytes: &'a [u8]) -> Option<Self> {
        Self::parse_up_to(bytes, MAX_RUNS)
    }

    /// [`Runs::parse`], for a table read out of guest memory, which may hold
    /// up to `max` runs.
    pub fn parse_up_to(bytes: &'a [u8], max: usize) -> Option<Self> {
        if bytes.len() < 8 {
            return None;
        }
        let count = u32::from_le_bytes(bytes[0..4].try_into().ok()?) as usize;
        if count == 0 || count > max || bytes.len() < encoded_len(count) {
            return None;
        }
        Some(Runs { bytes, count })
    }

    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = Run> + 'a {
        let bytes = self.bytes;
        (0..self.count).map(move |i| {
            let e = 8 + i * 16;
            Run {
                gpa: u64::from_le_bytes(bytes[e..e + 8].try_into().expect("checked in parse")),
                len: u64::from_le_bytes(bytes[e + 8..e + 16].try_into().expect("checked in parse")),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const N: usize = MAX_RUNS;
    static mut BUF: [u8; encoded_len(N)] = [0; encoded_len(N)];

    fn fill(r: &mut [Run]) {
        for (i, e) in r.iter_mut().enumerate() {
            *e = Run {
                gpa: (i as u64 + 1) * 4096,
                len: 4096,
            };
        }
    }

    #[test]
    fn a_table_survives_the_round_trip() {
        let mut all = [Run::default(); N];
        fill(&mut all);
        let mut buf = [0u8; encoded_len(N)];
        for n in [1, 2, 17, N] {
            let r = &all[..n];
            assert_eq!(encode(&mut buf, r), Some(encoded_len(n)));
            let parsed = Runs::parse(&buf).expect("a table just written");
            assert_eq!(parsed.len(), n);
            for (got, want) in parsed.iter().zip(r) {
                assert_eq!(got, *want);
            }
        }
    }

    #[test]
    fn more_runs_than_the_limit_are_not_encoded() {
        let too_many = [Run::default(); N + 1];
        // SAFETY: single-threaded test, and the buffer is only used here.
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(BUF) };
        assert_eq!(encode(&mut buf[..], &too_many), None);
    }

    /// A count that does not match the bytes is the shape a malformed block
    /// takes, and reading runs out of it would read whatever follows.
    #[test]
    fn a_count_longer_than_the_block_is_refused() {
        let mut all = [Run::default(); 2];
        fill(&mut all);
        let mut buf = [0u8; encoded_len(2)];
        encode(&mut buf, &all).unwrap();

        buf[0..4].copy_from_slice(&3u32.to_le_bytes());
        assert!(Runs::parse(&buf).is_none());

        buf[0..4].copy_from_slice(&(MAX_RUNS as u32 + 1).to_le_bytes());
        assert!(Runs::parse(&buf).is_none());
    }

    /// An empty table is not a registration of nothing, it is a message that
    /// says nothing, and the backend has no address to build from it.
    #[test]
    fn an_empty_table_is_refused() {
        assert!(Runs::parse(&[0u8; 8]).is_none());
        assert!(Runs::parse(&[]).is_none());
        assert!(Runs::parse(&[0u8; 4]).is_none());
    }

    /// The two sentinels have to stay distinct: a deep block read as the wrong
    /// kind is a parameter buffer read as addresses, or the reverse.
    #[test]
    fn the_sentinels_do_not_collide() {
        assert_ne!(PAGE_RUNS, crate::segments::SEGMENTED);
        assert_ne!(PAGE_RUNS_INDIRECT, crate::segments::SEGMENTED);
        assert_ne!(PAGE_RUNS_INDIRECT, PAGE_RUNS);
        // And none can be a real offset into a parameter block.
        assert!(PAGE_RUNS_INDIRECT > u16::MAX as u32);
    }

    /// The largest indirect table fits in the pages a direct table can name
    /// even when every one of them is scattered.
    #[test]
    fn an_indirect_table_fits_in_a_direct_one() {
        assert!(encoded_len(MAX_RUNS_INDIRECT) <= MAX_RUNS * 4096);
        assert!(encoded_len(MAX_RUNS_INDIRECT + 1) > MAX_RUNS * 4096);
    }

    #[test]
    fn a_long_table_parses_only_with_the_larger_bound() {
        const N1: usize = MAX_RUNS + 1;
        static mut LONG: [u8; encoded_len(N1)] = [0; encoded_len(N1)];
        // SAFETY: single-threaded test, and the buffer is only used here.
        let buf = unsafe { &mut *core::ptr::addr_of_mut!(LONG) };
        buf[0..4].copy_from_slice(&(N1 as u32).to_le_bytes());
        assert!(Runs::parse(&buf[..]).is_none());
        assert_eq!(
            Runs::parse_up_to(&buf[..], MAX_RUNS_INDIRECT).map(|r| r.len()),
            Some(N1)
        );
    }
}
