//! The host's page-run table: how the KMD names the guest-physical pages behind a
//! locked user buffer when an RM client registers memory by CPU address
//! (`NV01_MEMORY_SYSTEM_OS_DESCRIPTOR`, the `HELIOS_NVRM_OP_PIN` verb).
//!
//! RM reads such memory through an address in the *caller's* address space, which
//! for a guest means nothing on the host. So the guest pins the pages and sends
//! the pages themselves: the host builds an address of its own that aliases
//! exactly those pages. This is the format `host/backend/protocol/src/pageruns.rs`
//! reads and `guest/linux/conduit_gpu.c` (`nvgpu_emit_page_runs`) writes:
//!
//! ```text
//! u32 runs | u32 0 | runs x { u64 gpa, u64 len }     (little-endian)
//! ```
//!
//! Consecutive pages coalesce into one run, which is what keeps the table small: a
//! buffer the allocator gave out contiguously is one run however large it is. A
//! fully scattered one costs a run per page. Both `gpa` and `len` are whole pages.
//!
//! At most [`DIRECT_MAX_RUNS`] runs fit a message ("direct"). More than that, and
//! the table is left in guest memory instead and the message carries a table of
//! the runs *that table* occupies ("indirect", at most [`INDIRECT_MAX_RUNS`]).
//!
//! Pure functions of their arguments: the kernel side supplies the PFNs.

pub const PAGE_SIZE: u64 = 4096;
/// Bytes before the first run: `u32 runs`, `u32 reserved`.
pub const HEADER_BYTES: usize = 8;
/// Bytes of one run: `u64 gpa`, `u64 len`.
pub const RUN_BYTES: usize = 16;
/// Most runs a table carried inside the message may describe.
pub const DIRECT_MAX_RUNS: usize = 1024;
/// Most runs an indirect table may describe: as many as fit in the
/// `DIRECT_MAX_RUNS` pages a fully scattered direct table could name.
pub const INDIRECT_MAX_RUNS: usize = (DIRECT_MAX_RUNS * PAGE_SIZE as usize - HEADER_BYTES) / RUN_BYTES;

/// Bytes of a table of `runs` runs.
pub const fn table_bytes(runs: usize) -> usize {
    HEADER_BYTES + runs * RUN_BYTES
}

/// How many runs `pfns` coalesces into.
pub fn count_runs(pfns: &[u64]) -> usize {
    let mut runs = 0usize;
    let mut prev: Option<u64> = None;
    for &pfn in pfns {
        match prev {
            Some(p) if p.checked_add(1) == Some(pfn) => {}
            _ => runs += 1,
        }
        prev = Some(pfn);
    }
    runs
}

fn put32(out: &mut [u8], at: usize, v: u32) -> Option<()> {
    out.get_mut(at..at.checked_add(4)?)?.copy_from_slice(&v.to_le_bytes());
    Some(())
}

fn put64(out: &mut [u8], at: usize, v: u64) -> Option<()> {
    out.get_mut(at..at.checked_add(8)?)?.copy_from_slice(&v.to_le_bytes());
    Some(())
}

/// Write the table for `pfns` (page frame numbers of the locked pages, in buffer
/// order) into `out`, returning the bytes written. `None` if `pfns` is empty, a
/// frame number does not name an address, or `out` is too small — never a
/// truncated table, which would describe memory the caller did not ask for.
pub fn encode(pfns: &[u64], out: &mut [u8]) -> Option<usize> {
    if pfns.is_empty() {
        return None;
    }
    let mut runs = 0usize;
    let mut run_gpa = 0u64;
    let mut run_len = 0u64;
    let mut at = HEADER_BYTES;
    for &pfn in pfns {
        let gpa = pfn.checked_mul(PAGE_SIZE)?;
        if run_len != 0 && run_gpa.checked_add(run_len) == Some(gpa) {
            run_len += PAGE_SIZE;
            continue;
        }
        if run_len != 0 {
            put64(out, at, run_gpa)?;
            put64(out, at + 8, run_len)?;
            at += RUN_BYTES;
            runs += 1;
        }
        run_gpa = gpa;
        run_len = PAGE_SIZE;
    }
    put64(out, at, run_gpa)?;
    put64(out, at + 8, run_len)?;
    at += RUN_BYTES;
    runs += 1;
    put32(out, 0, u32::try_from(runs).ok()?)?;
    put32(out, 4, 0)?;
    Some(at)
}

/// The table for ONE physically contiguous range: what an indirect message carries
/// to say where the big table lives. `gpa` and `len` must be whole pages.
pub fn encode_single_run(gpa: u64, len: u64, out: &mut [u8]) -> Option<usize> {
    if len == 0 || gpa % PAGE_SIZE != 0 || len % PAGE_SIZE != 0 {
        return None;
    }
    put32(out, 0, 1)?;
    put32(out, 4, 0)?;
    put64(out, HEADER_BYTES, gpa)?;
    put64(out, HEADER_BYTES + 8, len)?;
    Some(table_bytes(1))
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec;
    use std::vec::Vec;

    fn rd32(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
    }
    fn rd64(b: &[u8], at: usize) -> u64 {
        let mut a = [0u8; 8];
        a.copy_from_slice(&b[at..at + 8]);
        u64::from_le_bytes(a)
    }

    #[test]
    fn limits_match_the_host() {
        // host/backend/protocol/src/pageruns.rs: MAX_RUNS = 1024,
        // MAX_RUNS_INDIRECT = (MAX_RUNS * 4096 - 8) / 16.
        assert_eq!(DIRECT_MAX_RUNS, 1024);
        assert_eq!(INDIRECT_MAX_RUNS, 262_143);
        assert_eq!(table_bytes(DIRECT_MAX_RUNS), 16_392);
    }

    #[test]
    fn contiguous_pages_are_one_run() {
        let pfns: Vec<u64> = (0x1000..0x1000 + 300).collect();
        assert_eq!(count_runs(&pfns), 1);
        let mut out = vec![0u8; table_bytes(1)];
        assert_eq!(encode(&pfns, &mut out), Some(table_bytes(1)));
        assert_eq!(rd32(&out, 0), 1);
        assert_eq!(rd32(&out, 4), 0);
        assert_eq!(rd64(&out, 8), 0x1000 * 4096);
        assert_eq!(rd64(&out, 16), 300 * 4096);
    }

    #[test]
    fn scattered_pages_are_a_run_each() {
        let pfns = [10u64, 12, 14, 16];
        assert_eq!(count_runs(&pfns), 4);
        let mut out = vec![0u8; table_bytes(4)];
        assert_eq!(encode(&pfns, &mut out), Some(table_bytes(4)));
        assert_eq!(rd32(&out, 0), 4);
        for (i, pfn) in pfns.iter().enumerate() {
            assert_eq!(rd64(&out, 8 + i * 16), pfn * 4096);
            assert_eq!(rd64(&out, 16 + i * 16), 4096);
        }
    }

    #[test]
    fn mixed_runs_split_where_frames_do() {
        // 5,6,7 | 20,21,22 | 9 : three runs; 7 -> 20 and 22 -> 9 are the breaks.
        let pfns = [5u64, 6, 7, 20, 21, 22, 9];
        assert_eq!(count_runs(&pfns), 3);
        let mut out = vec![0u8; table_bytes(3)];
        assert_eq!(encode(&pfns, &mut out), Some(table_bytes(3)));
        assert_eq!(rd32(&out, 0), 3);
        let want = [(5u64, 3u64), (20, 3), (9, 1)];
        for (i, (pfn, pages)) in want.iter().enumerate() {
            assert_eq!(rd64(&out, 8 + i * 16), pfn * 4096, "run {i} gpa");
            assert_eq!(rd64(&out, 16 + i * 16), pages * 4096, "run {i} len");
        }
    }

    #[test]
    fn descending_frames_do_not_coalesce() {
        let pfns = [9u64, 8, 7];
        assert_eq!(count_runs(&pfns), 3);
    }

    #[test]
    fn count_agrees_with_encode_on_a_long_pattern() {
        // groups of 3 contiguous frames, each group 10 frames after the last
        let mut pfns = Vec::new();
        for g in 0..500u64 {
            for k in 0..3 {
                pfns.push(g * 10 + k);
            }
        }
        let runs = count_runs(&pfns);
        assert_eq!(runs, 500);
        let mut out = vec![0u8; table_bytes(runs)];
        assert_eq!(encode(&pfns, &mut out), Some(table_bytes(runs)));
        assert_eq!(rd32(&out, 0) as usize, runs);
        assert_eq!(rd64(&out, 16), 3 * 4096);
    }

    #[test]
    fn too_small_an_output_is_none_never_a_truncated_table() {
        let pfns = [1u64, 3, 5];
        let mut small = vec![0u8; table_bytes(3) - 1];
        assert_eq!(encode(&pfns, &mut small), None);
        let mut exact = vec![0u8; table_bytes(3)];
        assert!(encode(&pfns, &mut exact).is_some());
    }

    #[test]
    fn empty_and_unaddressable_frames_are_refused() {
        let mut out = vec![0u8; 64];
        assert_eq!(encode(&[], &mut out), None);
        // a frame number whose address overflows u64
        assert_eq!(encode(&[u64::MAX], &mut out), None);
    }

    #[test]
    fn single_run_for_an_indirect_table() {
        let mut out = vec![0u8; table_bytes(1)];
        assert_eq!(encode_single_run(0x7000, 0x3000, &mut out), Some(table_bytes(1)));
        assert_eq!(rd32(&out, 0), 1);
        assert_eq!(rd64(&out, 8), 0x7000);
        assert_eq!(rd64(&out, 16), 0x3000);
        // not whole pages, or empty
        assert_eq!(encode_single_run(0x7001, 0x3000, &mut out), None);
        assert_eq!(encode_single_run(0x7000, 0x30, &mut out), None);
        assert_eq!(encode_single_run(0x7000, 0, &mut out), None);
    }
}
